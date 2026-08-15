//! `foo = { c d e }`: a name bound to a value that says nothing about the
//! arguments it takes.

use std::sync::{Arc, OnceLock};

use tree_sitter::Node;

use crate::line_struct::Span;

use super::{Command, Documentation, Param};

/// A value of no more than this many lines is shown in full on hover; a longer
/// one is reduced to its outline.
const LINES_SHOWN_IN_FULL: usize = 5;

/// What a name is bound to when nothing says what arguments it takes: `\foo`
/// substitutes its value and consumes nothing.
///
/// Where the binding was read from a file we hold the text of, the value's
/// [`Span`] travels with the name so hover can say what `\foo` stands for. The
/// value itself is never copied at parse time: the variable keeps the whole
/// file's text behind an `Arc` — the same allocation the document already has,
/// shared, not a slice of it duplicated per definition — and renders the
/// summary only when [`documentation`](Command::documentation) is called, which
/// for the overwhelming majority of definitions is never. That answer is then
/// cached, because the trait hands out a reference; the cache dies with the
/// `Variable`, which an edit rebuilds anyway.
///
/// A name known only as a name — a `lilypond-words` entry, an install binding
/// whose file the layer doesn't keep — carries no value and documents nothing.
pub struct Variable {
    name: String,
    value: Option<Value>,
    documentation: OnceLock<Option<Documentation>>,
}

/// Where a variable's value is written: the text of the file that binds it, and
/// the span of the value within that text.
struct Value {
    source: Arc<str>,
    span: Span,
}

impl Variable {
    /// A name with nothing behind it but the fact that it exists.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: None,
            documentation: OnceLock::new(),
        }
    }

    /// A name bound to the value written at `span` in `source`.
    pub fn bound_to(name: impl Into<String>, source: Arc<str>, span: Span) -> Self {
        Self {
            value: Some(Value { source, span }),
            ..Self::new(name)
        }
    }
}

impl Command for Variable {
    fn name(&self) -> &str {
        &self.name
    }

    fn signature(&self) -> &[Param] {
        &[]
    }

    fn documentation(&self) -> Option<&Documentation> {
        let value = self.value.as_ref()?;
        self.documentation
            .get_or_init(|| summarise(&self.name, value))
            .as_ref()
    }
}

/// Renders `name = value` for hover: the value in full where it is short enough
/// to read at a glance, otherwise its outline. `None` when the value is both
/// long and shapeless, where an outline would say nothing the name doesn't.
fn summarise(name: &str, value: &Value) -> Option<Documentation> {
    let text = value.source.get(value.span.start..value.span.end)?;
    let shown = if text.lines().count() <= LINES_SHOWN_IN_FULL {
        text.to_string()
    } else {
        outline(text)?
    };
    Some(Documentation {
        markdown: format!("```lilypond\n{name} = {shown}\n```"),
    })
}

/// A one-line sketch of `text`: everything but the contents of its blocks,
/// which become `…`.
///
/// So a value too long to show becomes `\relative c' { … }`, or
/// `{ \fixed c' { … } }` where the command sits inside a block — the wrapper
/// that says how to read the music, without the music. `None` when the sketch
/// is nothing *but* an ellipsis: a three-hundred-line `#(define …)` has no
/// shape to show, and a hover reading `foo = …` is worse than no hover at all.
///
/// Reparses `text` rather than keeping the nodes the document already had: a
/// value's span outlives by far the tree it was read from, and this runs at
/// most once per definition anyone actually hovers.
fn outline(text: &str) -> Option<String> {
    let tree = crate::document::parse(text, None);
    let root = tree.root_node();
    let mut cursor = root.walk();
    let nodes: Vec<Node> = root.children(&mut cursor).collect();
    let sketch = elide(&nodes, text);
    (sketch != ELLIPSIS).then_some(sketch)
}

const ELLIPSIS: &str = "…";

/// Renders a run of sibling nodes on one line, with the contents of every block
/// elided and any node that spans lines of its own replaced outright.
///
/// Takes the gaps between nodes from the source rather than joining with
/// spaces: the grammar is flat, so `c'` is two adjacent nodes and a space
/// between them would be a different pitch.
fn elide(nodes: &[Node], src: &str) -> String {
    let mut out = String::new();
    let mut previous_end = None;
    for node in nodes {
        if let Some(end) = previous_end {
            out.push_str(gap(&src[end..node.start_byte()]));
        }
        out.push_str(&elide_node(*node, src));
        previous_end = Some(node.end_byte());
    }
    out
}

/// The whitespace between two nodes, collapsed: any of it at all becomes one
/// space, so a value broken over several lines still reads as one.
fn gap(between: &str) -> &'static str {
    if between.is_empty() { "" } else { " " }
}

fn elide_node(node: Node, src: &str) -> String {
    if let Some((open, inner, close)) = block_parts(node, src) {
        return format!("{open} {} {close}", inner_sketch(&inner, src));
    }
    let text = text_of(node, src);
    if text.contains('\n') {
        ELLIPSIS.to_string()
    } else {
        text.to_string()
    }
}

/// What to show inside a block: the single wrapper it holds, elided in its
/// turn, so `{ \fixed c' { … } }` keeps the one thing the outer braces would
/// otherwise hide. Anything else becomes `…`, since showing part of a block's
/// contents would read as all of them.
fn inner_sketch(inner: &[Node], src: &str) -> String {
    if is_wrapped_music(inner, src) {
        elide(inner, src)
    } else {
        ELLIPSIS.to_string()
    }
}

/// The commands that say how to *read* the music they wrap rather than adding
/// anything of their own, and so are worth keeping when the music itself goes.
///
/// A closed list, deliberately. Outside a block there is no need for one: the
/// value is shown whole, whatever it turns out to be. Inside one, the flat
/// grammar can't tell `\transpose c d \relative c' { … }`, one wrapper around
/// another, from `\clef bass \relative c { … }`, a command and then some music
/// — telling those apart needs each command's arity, which is the note
/// analyser's job and not something hover should be reaching into.
const WRAPPERS: &[&str] = &[
    "\\transpose",
    "\\relative",
    "\\fixed",
    "\\notemode",
    "\\notes",
    "\\chordmode",
    "\\chords",
    "\\drummode",
    "\\drums",
    "\\figuremode",
    "\\figures",
    "\\lyricmode",
    "\\lyrics",
];

/// Whether a run of nodes is music under a [`WRAPPERS`] command: the wrapper
/// first, its arguments, and one block last with nothing following it.
fn is_wrapped_music(nodes: &[Node], src: &str) -> bool {
    nodes
        .first()
        .is_some_and(|node| WRAPPERS.contains(&text_of(*node, src)))
        && nodes.iter().filter(|node| is_block(node)).count() == 1
        && nodes.last().is_some_and(is_block)
}

fn is_block(node: &Node) -> bool {
    matches!(node.kind(), "expression_block" | "parallel_music")
}

/// A block's delimiters and the nodes between them; `None` for a node that
/// isn't a block.
fn block_parts<'t>(node: Node<'t>, src: &'t str) -> Option<(&'t str, Vec<Node<'t>>, &'t str)> {
    if !is_block(&node) {
        return None;
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();
    let (open, rest) = children.split_first()?;
    let (close, inner) = rest.split_last()?;
    Some((text_of(*open, src), inner.to_vec(), text_of(*close, src)))
}

fn text_of<'t>(node: Node, src: &'t str) -> &'t str {
    &src[node.start_byte()..node.end_byte()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    /// What hover would show for `name` in `src`, with the code fence stripped:
    /// read through a whole [`Document`], since finding the value is half the
    /// behaviour under test.
    fn shown(src: &str, name: &str) -> Option<String> {
        let document = Document::new(src.to_string());
        let command = document.commands_defined().get(name).expect("bound");
        let markdown = command.documentation()?.markdown.clone();
        Some(
            markdown
                .trim_start_matches("```lilypond\n")
                .trim_end_matches("\n```")
                .to_string(),
        )
    }

    /// `body` on `lines` lines, to push a value past what is shown in full.
    fn long(lines: usize) -> String {
        vec!["c d e f"; lines].join("\n")
    }

    #[test]
    fn a_short_value_is_shown_in_full() {
        assert_eq!(
            shown("myvar = \\relative c' { c d e }\n", "myvar").as_deref(),
            Some("myvar = \\relative c' { c d e }")
        );
    }

    #[test]
    fn a_value_of_five_lines_is_still_shown_in_full() {
        let src = format!("myvar = {{\n{}\n}}\n", long(3));
        assert_eq!(
            shown(&src, "myvar").as_deref(),
            Some(format!("myvar = {{\n{}\n}}", long(3)).as_str())
        );
    }

    #[test]
    fn a_long_value_keeps_the_command_and_loses_the_music() {
        let src = format!("myvar = \\relative c' {{\n{}\n}}\n", long(9));
        assert_eq!(
            shown(&src, "myvar").as_deref(),
            Some("myvar = \\relative c' { … }")
        );
    }

    #[test]
    fn a_long_value_in_a_mode_keeps_the_mode() {
        let src = format!("mylyrics = \\lyricmode {{\n{}\n}}\n", long(9));
        assert_eq!(
            shown(&src, "mylyrics").as_deref(),
            Some("mylyrics = \\lyricmode { … }")
        );
    }

    #[test]
    fn a_command_wrapped_in_a_block_is_kept_too() {
        let src = format!("myvar = {{\n  \\fixed c' {{\n{}\n}}\n}}\n", long(9));
        assert_eq!(
            shown(&src, "myvar").as_deref(),
            Some("myvar = { \\fixed c' { … } }")
        );
    }

    #[test]
    fn a_transposition_keeps_both_its_pitches() {
        let src = format!(
            "myvar = \\transpose c d \\relative c' {{\n{}\n}}\n",
            long(9)
        );
        assert_eq!(
            shown(&src, "myvar").as_deref(),
            Some("myvar = \\transpose c d \\relative c' { … }")
        );
    }

    #[test]
    fn a_block_holding_more_than_one_command_is_elided_whole() {
        // Showing the first of several commands would read as the whole of the
        // block's contents, which is worse than admitting to hiding them.
        let src = format!(
            "myvar = {{\n  \\clef bass\n  \\relative c {{\n{}\n}}\n}}\n",
            long(9)
        );
        assert_eq!(shown(&src, "myvar").as_deref(), Some("myvar = { … }"));
    }

    #[test]
    fn a_long_value_with_no_shape_at_all_says_nothing() {
        let lines = ["  (display \"la\")"; 9].join("\n");
        let src = format!("myvar = #(begin\n{lines})\n");
        assert_eq!(shown(&src, "myvar"), None);
    }

    #[test]
    fn a_name_bound_to_nothing_we_can_see_says_nothing() {
        // The word list and the install layer bind names without keeping the
        // text they were read from; hover falls silent rather than guessing.
        let variable = Variable::new("break");
        assert!(variable.documentation().is_none());
    }
}
