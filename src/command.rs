//! Command/argument parsing.
//!
//! LilyPond commands (`\repeat`, `\volta`, `\relative`, `\key`, …) take
//! arguments in command-specific shapes that the tree-sitter grammar leaves as a
//! flat run of sibling nodes — `\repeat volta 2 { … }` is an `escaped_word`, a
//! `symbol`, an `unsigned_integer` and an `expression_block`, all siblings. This
//! module recognises a command from its `escaped_word` and consumes the
//! arguments its signature calls for, producing a structured [`CommandCall`] that
//! the note analyser, the refactorings and (later) a completion provider can
//! share instead of each re-deriving the shape ad hoc.
//!
//! Only the commands needed today carry a [`CommandSpec`]. Adding `\relative`,
//! `\tempo`, `\key` and friends later is a matter of registering their argument
//! shapes in [`SPECS`] and teaching [`ArgKind`] any new argument form they need.

use tree_sitter::Node;

use crate::line_struct::Span;

/// The shape of one argument a command expects, in the order it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// A bare word naming a variant — the `volta` of `\repeat volta 2`. A
    /// `symbol` in the grammar, meaningful only as this argument (contrast the
    /// `escaped_word` of a command itself).
    BareWord,
    /// A single unsigned integer — the `2` of `\repeat volta 2`.
    Count,
    /// A comma-separated list of unsigned integers — the `2,3` of `\volta 2,3`.
    NumberList,
    /// A music expression: a `{ … }` or `<< … >>` block, or a single braceless
    /// note or chord (`\repeat percent 4 c2`).
    Music,
    // Argument forms the foreseeable commands will need, not yet parsed:
    //   Pitch        — \relative, \fixed, \key       (a note name with octave marks)
    //   Word         — the \major of \key c \major   (an escaped_word argument)
    //   String       — \clef, \language              (a quoted string)
    //   PropertyPath — \set, \unset                  (Staff.instrumentName)
    // Add the variant here and its consumption in `consume_arg` when the first
    // command that needs it is registered.
}

/// A command's argument signature: its name (without the backslash) and the
/// arguments it consumes, in order.
pub struct CommandSpec {
    pub name: &'static str,
    pub args: &'static [ArgKind],
}

/// The argument signatures we recognise. The note analyser still handles the
/// mode-changing commands (`\relative`, `\chordmode`, …) itself for now; this
/// table grows as those are migrated onto the shared parser.
static SPECS: &[CommandSpec] = &[
    CommandSpec {
        name: "repeat",
        args: &[ArgKind::BareWord, ArgKind::Count, ArgKind::Music],
    },
    CommandSpec {
        name: "volta",
        args: &[ArgKind::NumberList, ArgKind::Music],
    },
    CommandSpec {
        name: "alternative",
        args: &[ArgKind::Music],
    },
];

/// The argument signature for `\name` (given without the backslash), if known.
pub fn spec_for(name: &str) -> Option<&'static CommandSpec> {
    SPECS.iter().find(|spec| spec.name == name)
}

/// A parsed command invocation: the command word and the arguments it consumed,
/// each with the source extent it covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCall {
    /// The command name, without the leading backslash (`repeat`).
    pub name: String,
    /// The `\command` keyword token.
    pub keyword: Span,
    /// The whole call: the keyword through the last argument actually consumed.
    pub span: Span,
    /// The arguments parsed, in order. Shorter than the signature when the source
    /// is mid-edit and an argument is still missing.
    pub args: Vec<Arg>,
}

impl CommandCall {
    /// The header span: the keyword and every non-[`Music`](Arg::Music) argument
    /// before the body — the `\repeat volta 2` a cursor sits in to invoke an
    /// action, excluding the `{ … }` body.
    pub fn header(&self) -> Span {
        let end = self
            .args
            .iter()
            .take_while(|arg| !matches!(arg, Arg::Music { .. }))
            .map(Arg::span)
            .last()
            .map_or(self.keyword.end, |span| span.end);
        Span::new(self.keyword.start, end)
    }

    /// The span of this call's first [`Music`](Arg::Music) argument, its body.
    pub fn body(&self) -> Option<Span> {
        self.args.iter().find_map(|arg| match arg {
            Arg::Music { span } => Some(*span),
            _ => None,
        })
    }
}

/// One parsed argument, tagged by its [`ArgKind`] and carrying the source extent
/// it covered together with its decoded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    BareWord { span: Span, text: String },
    Count { span: Span, value: u32 },
    NumberList { span: Span, values: Vec<u32> },
    Music { span: Span },
}

impl Arg {
    pub fn span(&self) -> Span {
        match self {
            Arg::BareWord { span, .. }
            | Arg::Count { span, .. }
            | Arg::NumberList { span, .. }
            | Arg::Music { span } => *span,
        }
    }
}

/// Parses the command at `children[start]` (an `escaped_word`) against its
/// signature, if it has one, consuming as many of its arguments as are present.
/// Returns the structured call and the index of the first node after the
/// arguments consumed. `None` when the word names no known command, leaving the
/// caller to handle it as it did before.
///
/// Arguments are taken from consecutive siblings; consumption stops at the first
/// argument whose node isn't where the signature expects it, so a half-typed
/// `\repeat volta` yields a call with the arguments seen so far rather than
/// nothing.
pub fn parse(children: &[Node], start: usize, src: &str) -> Option<(CommandCall, usize)> {
    let keyword_node = *children.get(start)?;
    if keyword_node.kind() != "escaped_word" {
        return None;
    }
    let name = src[keyword_node.start_byte()..keyword_node.end_byte()].strip_prefix('\\')?;
    let spec = spec_for(name)?;

    let keyword = node_span(keyword_node);
    let mut args = Vec::new();
    let mut i = start + 1;
    for &kind in spec.args {
        let Some((arg, next)) = consume_arg(kind, children, i, src) else {
            break;
        };
        args.push(arg);
        i = next;
    }

    let end = args.last().map_or(keyword.end, |arg| arg.span().end);
    Some((
        CommandCall {
            name: name.to_string(),
            keyword,
            span: Span::new(keyword.start, end),
            args,
        },
        i,
    ))
}

/// Consumes one argument of the given `kind` starting at `children[i]`, returning
/// it and the next index, or `None` if the expected node isn't there.
fn consume_arg(kind: ArgKind, children: &[Node], i: usize, src: &str) -> Option<(Arg, usize)> {
    let node = *children.get(i)?;
    match kind {
        ArgKind::BareWord if node.kind() == "symbol" => {
            let span = node_span(node);
            let text = src[span.start..span.end].to_string();
            Some((Arg::BareWord { span, text }, i + 1))
        }
        ArgKind::Count if node.kind() == "unsigned_integer" => {
            let span = node_span(node);
            let value = src[span.start..span.end].parse().unwrap_or(0);
            Some((Arg::Count { span, value }, i + 1))
        }
        ArgKind::NumberList => consume_number_list(children, i, src),
        ArgKind::Music if is_block(node.kind()) => Some((
            Arg::Music {
                span: node_span(node),
            },
            i + 1,
        )),
        ArgKind::Music => consume_single_music(children, i),
        _ => None,
    }
}

/// Consumes a braceless music argument — a single note or chord written without
/// surrounding braces, as in `\repeat percent 4 c2`. The grammar leaves a note's
/// tokens as a flat run of byte-adjacent siblings (`c`, `2`, `.`, `->`, …) with
/// the next event whitespace-separated, so we take the leading `symbol`/`chord`
/// and every sibling butting directly against it. `None` if the next node is
/// neither a note symbol nor a chord (a block is handled before this).
fn consume_single_music(children: &[Node], start: usize) -> Option<(Arg, usize)> {
    let first = *children.get(start)?;
    if first.kind() != "symbol" && first.kind() != "chord" {
        return None;
    }
    let mut end = first.end_byte();
    let mut i = start + 1;
    while let Some(node) = children.get(i).filter(|n| n.start_byte() == end) {
        end = node.end_byte();
        i += 1;
    }
    Some((
        Arg::Music {
            span: Span::new(first.start_byte(), end),
        },
        i,
    ))
}

/// Consumes a comma-separated run of unsigned integers (`2,3`) starting at
/// `children[start]`, or `None` if the first node isn't an integer.
fn consume_number_list(children: &[Node], start: usize, src: &str) -> Option<(Arg, usize)> {
    let first = *children.get(start)?;
    if first.kind() != "unsigned_integer" {
        return None;
    }
    let mut values = vec![number(first, src)];
    let mut end = first.end_byte();
    let mut i = start + 1;
    // Each further `, n` extends the list; a trailing comma with no integer is
    // left for the caller.
    while children.get(i).is_some_and(|n| is_comma(*n, src))
        && let Some(n) = children
            .get(i + 1)
            .filter(|n| n.kind() == "unsigned_integer")
    {
        values.push(number(*n, src));
        end = n.end_byte();
        i += 2;
    }
    Some((
        Arg::NumberList {
            span: Span::new(first.start_byte(), end),
            values,
        },
        i,
    ))
}

fn number(node: Node, src: &str) -> u32 {
    src[node.start_byte()..node.end_byte()].parse().unwrap_or(0)
}

fn node_span(node: Node) -> Span {
    Span::new(node.start_byte(), node.end_byte())
}

/// Whether a node kind is a music block: a `{ … }` expression or a `<< … >>`
/// parallel-music block. The two ways music groups in the grammar.
pub(crate) fn is_block(kind: &str) -> bool {
    kind == "expression_block" || kind == "parallel_music"
}

fn is_comma(node: Node, src: &str) -> bool {
    node.kind() == "punctuation" && &src[node.start_byte()..node.end_byte()] == ","
}

/// The source-ordered command calls found in a document, queryable by the
/// position or span a refactoring is working at. Mirrors [`Events`] so a call
/// can be located by binary search.
///
/// [`Events`]: crate::notes::Events
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Commands {
    /// In source order by keyword start. A call's body may contain later calls
    /// (a `\volta` inside a `\repeat`), so spans nest rather than stay disjoint.
    calls: Vec<CommandCall>,
}

impl Commands {
    pub fn new(calls: Vec<CommandCall>) -> Self {
        debug_assert!(
            calls
                .windows(2)
                .all(|w| w[0].keyword.start <= w[1].keyword.start),
            "command calls must be in source order by keyword start"
        );
        Self { calls }
    }

    /// The calls whose keyword lies within the half-open byte range `start..end`
    /// — the `\volta`/`\alternative` calls inside a `\repeat` body, say.
    pub fn within(&self, start: usize, end: usize) -> impl Iterator<Item = &CommandCall> {
        self.calls
            .iter()
            .filter(move |call| start <= call.keyword.start && call.keyword.start < end)
    }
}

impl std::ops::Deref for Commands {
    type Target = [CommandCall];

    fn deref(&self) -> &Self::Target {
        &self.calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Tree;

    fn tree(src: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
            .expect("load grammar");
        parser.parse(src, None).expect("parse")
    }

    /// Parses the first command at the top level of `src`.
    fn call(src: &str) -> Option<CommandCall> {
        let tree = tree(src);
        let root = tree.root_node();
        let mut cursor = root.walk();
        let children: Vec<Node> = root.children(&mut cursor).collect();
        let start = children.iter().position(|n| n.kind() == "escaped_word")?;
        parse(&children, start, src).map(|(call, _)| call)
    }

    #[test]
    fn repeat_has_kind_count_and_body() {
        let src = "\\repeat volta 2 { c d }";
        let call = call(src).expect("a repeat call");
        assert_eq!(call.name, "repeat");
        assert!(matches!(&call.args[0], Arg::BareWord { text, .. } if text == "volta"));
        assert!(matches!(call.args[1], Arg::Count { value: 2, .. }));
        assert!(matches!(call.args[2], Arg::Music { .. }));
        // The header stops before the body; the body is the brace block.
        assert_eq!(
            &src[call.header().start..call.header().end],
            "\\repeat volta 2"
        );
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "{ c d }");
    }

    #[test]
    fn unfold_is_a_repeat_kind_too() {
        let call = call("\\repeat unfold 4 { c }").expect("a repeat call");
        assert!(matches!(&call.args[0], Arg::BareWord { text, .. } if text == "unfold"));
        assert!(matches!(call.args[1], Arg::Count { value: 4, .. }));
    }

    #[test]
    fn volta_reads_a_number_list() {
        let call = call("\\volta 1,2,3 { c }").expect("a volta call");
        assert_eq!(call.name, "volta");
        let Arg::NumberList { values, .. } = &call.args[0] else {
            panic!("expected a number list, got {:?}", call.args[0]);
        };
        assert_eq!(values, &[1, 2, 3]);
    }

    #[test]
    fn volta_with_a_single_number() {
        let call = call("\\volta 2 { c }").expect("a volta call");
        assert!(matches!(&call.args[0], Arg::NumberList { values, .. } if values == &[2]));
    }

    #[test]
    fn alternative_takes_one_music_block() {
        let src = "\\alternative { { a } { b } }";
        let call = call(src).expect("an alternative call");
        assert_eq!(call.name, "alternative");
        assert_eq!(call.args.len(), 1);
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "{ { a } { b } }");
    }

    #[test]
    fn music_can_be_a_braceless_note() {
        // `\repeat percent 4 c2` takes a single note as its music argument, with
        // no surrounding braces; the body spans the whole note including duration.
        let src = "\\repeat percent 4 c2";
        let call = call(src).expect("a repeat call");
        assert!(matches!(call.args[1], Arg::Count { value: 4, .. }));
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "c2");
    }

    #[test]
    fn music_can_be_a_braceless_chord() {
        // A single `<…>` chord with its duration is a braceless music argument too.
        let src = "\\repeat percent 4 <c e>2";
        let call = call(src).expect("a repeat call");
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "<c e>2");
    }

    #[test]
    fn braceless_music_stops_at_the_next_event() {
        // The single note ends where the next, whitespace-separated note begins;
        // the trailing `d8` is left for the caller.
        let src = "{ \\repeat unfold 2 c4 d8 }";
        let tree = tree(src);
        let root = tree.root_node();
        let block = root.child(0).expect("a block");
        let mut cursor = block.walk();
        let children: Vec<Node> = block.children(&mut cursor).collect();
        let start = children
            .iter()
            .position(|n| n.kind() == "escaped_word")
            .unwrap();
        let (call, _) = parse(&children, start, src).expect("a repeat call");
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "c4");
    }

    #[test]
    fn a_missing_argument_stops_consumption() {
        // Half-typed `\repeat volta` with no count or body yet: the kind is read,
        // and the call simply has no further arguments.
        let call = call("\\repeat volta").expect("a partial repeat call");
        assert_eq!(call.args.len(), 1);
        assert!(call.body().is_none());
    }

    #[test]
    fn an_unregistered_command_is_not_parsed() {
        // `\transpose` has no signature yet, so the parser declines it.
        let tree = tree("\\transpose c d { e }");
        let root = tree.root_node();
        let mut cursor = root.walk();
        let children: Vec<Node> = root.children(&mut cursor).collect();
        assert!(parse(&children, 0, "\\transpose c d { e }").is_none());
    }
}
