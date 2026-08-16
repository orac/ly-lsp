//! Reading two things a document's context vocabulary is made of, each its
//! own namespace, separate from commands: the context *types* `\context { …
//! }` blocks declare ([`ContextType`], [`read`]), and the context *instance*
//! names `\new`/`\context` create ([`ContextInstance`], [`read_instances`]).
//!
//! `\layout { \context { \name Staff … } }` teaches LilyPond a new — or
//! modified — context type. This is not a command in the sense
//! [`command`](crate::command) means it: `Staff` is never written with a
//! leading backslash, and a block that names one carries no arguments a
//! `\foo` call would. It has its own vocabulary of directives
//! (`\name`, `\alias`, `\accepts`, `\consists`, …) that only mean something
//! inside a `\context { … }` block, so it gets its own reader rather than
//! being folded into `command::scheme` or the assignment-based one in
//! [`document`](crate::document).
//!
//! `\new Voice = "vocals" { … }` and `\context Voice = "vocals" { … }`, by
//! contrast, sit inside ordinary music rather than a `\layout` block, and
//! parse as a `named_context` node the grammar tells apart from a `\context {
//! … }` declaration entirely — see [`read_instances`] for that shape, and for
//! why `\change`, which refers to an instance name rather than creating one,
//! is deliberately not read here.
//!
//! # Not every `\context { … }` block is a declaration
//!
//! `\context { \Staff \override NoteHead.color = #red }` *modifies* an
//! existing context type rather than declaring one — there is no `\name` to
//! give it. And `\new Staff = "x" { … }` / `\context Staff = "x" { … }`,
//! which *invoke* a context in a score, parse as a `named_context` node the
//! grammar tells apart from this shape entirely, so there is no risk of
//! confusing the two. Only a block containing `\name` becomes a
//! [`ContextType`].

use std::sync::OnceLock;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Query, QueryCursor, Tree};

use crate::command::scheme::texinfo_to_markdown;
use crate::line_struct::Span;

/// One context type declared by a `\context { … }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextType {
    /// From `\name`: `Staff` in `\name Staff`, `MyStaff` in `\name "MyStaff"`.
    pub name: String,
    /// Every `\alias` the block carries, in source order. A context can have
    /// several — `PianoStaff` aliases `GrandStaff`, for instance.
    pub aliases: Vec<String>,
    /// From `\description`, converted from Texinfo to Markdown the same way
    /// a music function's docstring is (see
    /// [`scheme::texinfo_to_markdown`](crate::command::scheme::texinfo_to_markdown)).
    pub description: Option<String>,
    /// The extent of the name as written after `\name` — the string's
    /// contents where it's quoted, not the quotes themselves, so this always
    /// spans exactly `name`. What go-to-definition will land on.
    pub name_span: Span,
    /// The extent of the whole `\context { … }` block, `\context` keyword
    /// included — what a later step wants for hover, and for deciding which
    /// block the cursor is in.
    pub block_span: Span,
}

/// Matches an `escaped_word` immediately followed by an `expression_block`,
/// wherever in the tree that occurs. Unlike
/// [`SYMBOL_QUERY`](crate::document::SYMBOL_QUERY), which is anchored on
/// `lilypond_program` because a top-level assignment is the only definition
/// it wants, a `\context` block is always nested — inside `\layout` or
/// `\midi`, themselves usually inside `\score` — so this pattern is wrapped
/// in `(_ …)` rather than a named parent, matching under any node at any
/// depth. The keyword's text is checked in Rust rather than with a `#eq?`
/// predicate, matching how [`document::extract`](crate::document) tells
/// `\include` apart from an ordinary reference after a similarly broad
/// capture.
const CONTEXT_QUERY: &str = r#"
(_ (escaped_word) @keyword . (expression_block) @block)
"#;

fn context_query() -> &'static Query {
    static QUERY: OnceLock<Query> = OnceLock::new();
    QUERY.get_or_init(|| Query::new(&language(), CONTEXT_QUERY).expect("valid query"))
}

/// The LilyPond grammar as a tree-sitter [`Language`]. A private copy of
/// [`document::language`](crate::document) rather than a shared one: this
/// module isn't allowed to touch `document.rs`, and the call is a one-liner.
fn language() -> Language {
    tree_sitter_lilypond::LANGUAGE_LILYPOND.into()
}

/// Reads every context type `tree` declares, in source order.
pub fn read(tree: &Tree, src: &str) -> Vec<ContextType> {
    let query = context_query();
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();

    let mut matches = cursor.matches(query, tree.root_node(), src.as_bytes());
    while let Some(m) = matches.next() {
        let mut keyword = None;
        let mut block = None;
        for cap in m.captures {
            match capture_names[cap.index as usize] {
                "keyword" => keyword = Some(cap.node),
                "block" => block = Some(cap.node),
                _ => {}
            }
        }
        let (Some(keyword), Some(block)) = (keyword, block) else {
            continue;
        };
        if text(keyword, src) != "\\context" {
            continue;
        }
        if let Some(context) = read_block(keyword, block, src) {
            out.push(context);
        }
    }
    out
}

/// Reads one `\context { … }` block, given its keyword and the block itself.
/// `None` where the block has no `\name` — it modifies an existing type
/// rather than declaring one.
fn read_block(keyword: Node, block: Node, src: &str) -> Option<ContextType> {
    let children = named_children(block);

    let mut name = None;
    let mut aliases = Vec::new();
    let mut description = None;

    // The grammar leaves `\name X`, `\alias Y` and `\description "…"` as flat
    // runs of sibling nodes — nothing groups a directive with its value — so
    // this pairs them up by hand rather than matching a nested shape.
    //
    // Not every directive takes a value: `\context { \Staff \name MyStaff }`
    // inherits a whole type by naming it, and `\Staff` is followed straight
    // away by the next directive. So a directive claims the following node
    // only if it isn't itself an `escaped_word`, and the walk advances one
    // node at a time — a value is a `symbol` or a `string`, never something
    // this match would mistake for a directive.
    for (i, &directive) in children.iter().enumerate() {
        if directive.kind() != "escaped_word" {
            continue;
        }
        let value = children
            .get(i + 1)
            .copied()
            .filter(|next| next.kind() != "escaped_word");
        match text(directive, src) {
            "\\name" => {
                if let Some(found) = value.and_then(|v| name_value(v, src)) {
                    name = Some(found);
                }
            }
            "\\alias" => {
                if let Some((alias, _)) = value.and_then(|v| name_value(v, src)) {
                    aliases.push(alias);
                }
            }
            "\\description" => {
                if let Some(v) = value.filter(|v| v.kind() == "string") {
                    description = Some(texinfo_to_markdown(&string_value(v, src)));
                }
            }
            _ => {}
        }
    }

    let (name, name_span) = name?;
    Some(ContextType {
        name,
        aliases,
        description,
        name_span,
        block_span: Span::new(keyword.start_byte(), block.end_byte()),
    })
}

/// The name a `\name` or `\alias` value node stands for, and the span of the
/// name as written: the bare word itself for `symbol`, or a quoted string's
/// contents (not its quotes) for `string`. `None` for anything else, which
/// is what a half-typed `\name` with nothing after it yet looks like.
fn name_value(node: Node, src: &str) -> Option<(String, Span)> {
    match node.kind() {
        "symbol" => Some((
            text(node, src).to_string(),
            Span::new(node.start_byte(), node.end_byte()),
        )),
        "string" => {
            let span = string_contents_span(node)?;
            Some((string_value(node, src), span))
        }
        _ => None,
    }
}

/// The span between a `string` node's quotes: its first named child through
/// its last. The quote tokens themselves are anonymous nodes, so the named
/// children — `string_fragment` and `escape_sequence` runs — are exactly the
/// contents, and are contiguous in the source whether or not there are any
/// escapes to split them apart.
fn string_contents_span(string: Node) -> Option<Span> {
    let children = named_children(string);
    let first = children.first()?;
    let last = children.last().unwrap_or(first);
    Some(Span::new(first.start_byte(), last.end_byte()))
}

/// The text a `string` node stands for, with its escapes decoded. The
/// counterpart of
/// [`scheme::string_value`](crate::command::scheme) for a plain LilyPond
/// string rather than an embedded Scheme one: the grammar names the two
/// node kinds differently (`string_fragment`/`escape_sequence` here, the
/// `scheme_`-prefixed pair there), so the reader can't be shared, small as
/// it is.
fn string_value(string: Node, src: &str) -> String {
    let mut out = String::new();
    for child in named_children(string) {
        match child.kind() {
            "string_fragment" => out.push_str(text(child, src)),
            "escape_sequence" => out.push_str(unescape(text(child, src))),
            _ => {}
        }
    }
    out
}

/// What one `\x` escape stands for, mirroring
/// [`scheme::unescape`](crate::command::scheme)'s handling of the analogous
/// Scheme escape.
fn unescape(escape: &str) -> &str {
    match escape.strip_prefix('\\').unwrap_or(escape) {
        "n" => "\n",
        "t" => "\t",
        "r" => "\r",
        other => other,
    }
}

/// One context *instance* name created by `\new`/`\context` — the
/// `"vocals"` of `\new Voice = "vocals" { … }` — as a namespace of its own,
/// separate from both commands and [`ContextType`]s.
///
/// # A file-scoped approximation
///
/// An instance name really belongs to the music expression that creates it:
/// two unrelated `\new Voice = "vocals"` calls in the same file, in music
/// that never nests one inside the other, name two different contexts that
/// merely share a spelling. Tracking one [`ContextInstance`] per *name*,
/// file-wide, ignores that — the same simplification [`ContextType`] and
/// every command definition already make for the vocabulary they live in.
/// It's what makes completion and go-to-definition useful without a model of
/// score structure; the price is that a `\change` or `\lyricsto` naming a
/// duplicated instance name resolves to whichever occurrence [`read_instances`]
/// saw last, which need not be the one actually in scope at that point in the
/// music. Worth revisiting only once a real score is observed to collide this
/// way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextInstance {
    /// The name as written — `vocals` whether written `"vocals"` or bare.
    pub name: String,
    /// The context type it was created as — `Voice` in `\new Voice =
    /// "vocals"`. `None` where [`read_named_context`] found a `named_context`
    /// node whose second child wasn't the plain `symbol` a context type
    /// always is — a half-typed `\new = "vocals"` mid-edit, say.
    pub type_name: Option<String>,
    /// The name as written, quotes excluded where it was quoted — what
    /// go-to-definition lands on, mirroring [`ContextType::name_span`].
    pub span: Span,
}

/// Matches every `\new`/`\context` invocation in the tree: the grammar folds
/// the keyword and its context type into one `named_context` node (see the
/// module doc on [`command::parse`](crate::command::parse) for the fuller
/// story of that shape), so a plain node-kind query finds every one without
/// needing to tell `\new` apart from `\context` — both create an instance the
/// same way.
///
/// `\change Staff = "lower"` deliberately doesn't match here: LilyPond's own
/// grammar never wraps it in a `named_context` (see the module doc above
/// [`read_instances`]), and — as that doc also explains — it wouldn't belong
/// in this reader's output even if it did, since it refers to an instance
/// rather than creating one.
const NAMED_CONTEXT_QUERY: &str = "(named_context) @named_context";

fn named_context_query() -> &'static Query {
    static QUERY: OnceLock<Query> = OnceLock::new();
    QUERY.get_or_init(|| Query::new(&language(), NAMED_CONTEXT_QUERY).expect("valid query"))
}

/// Reads every context instance name `tree`'s `\new`/`\context` calls create,
/// in source order.
///
/// # Why `\change` isn't read here
///
/// `\change Staff = "lower"` moves the music that follows into an
/// *already-existing* context of type `Staff` named `"lower"` — it never
/// brings one into being, the way `\new`'s own documentation says outright
/// ("creates a fresh context") and `\context`'s says by contrast ("finds the
/// existing context… creating one only if none is found"). `\change` has
/// neither verb: LilyPond's manual describes it purely as changing which
/// context an interpretation context is currently pointed at. So it is a
/// *reference* to a name this reader must already know, not a second source
/// of definitions for it — matching how [`Scope`](crate::vocabulary::Scope)'s
/// other namespaces work: a command *call* isn't read as a fresh command
/// definition either. `\change`'s target uses the namespace through
/// [`ArgKind::ContextName`](crate::command::ArgKind::ContextName)'s parsed
/// span, the same way any other reference to a name works, and
/// go-to-definition resolves it from there — not through a second entry
/// point into this function.
pub fn read_instances(tree: &Tree, src: &str) -> Vec<ContextInstance> {
    let query = named_context_query();
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();

    let mut matches = cursor.matches(query, tree.root_node(), src.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            if let Some(instance) = read_named_context(cap.node, src) {
                out.push(instance);
            }
        }
    }
    out
}

/// Reads one `named_context` node for the instance it creates. `None` where
/// the call names no instance at all — `\new Voice { c }` with no `= "name"`
/// creates a context but never names it, so there is nothing here for
/// go-to-definition to land on, the same reasoning [`read_block`] applies to
/// an unnamed `\context { … }` type declaration.
///
/// The grammar leaves a `named_context` node's children as a flat run —
/// `escaped_word` (`\new`/`\context`), `symbol` (the type), and, only where a
/// name was written, a `punctuation` `=` followed by a `symbol` or `string` —
/// so this reads them positionally rather than by a nested shape, the same
/// choice [`read_block`] makes for a `\context { … }` block's directives.
fn read_named_context(node: Node, src: &str) -> Option<ContextInstance> {
    let children = named_children(node);
    let type_name = children
        .get(1)
        .filter(|n| n.kind() == "symbol")
        .map(|n| text(*n, src).to_string());
    let value = children
        .iter()
        .position(|n| n.kind() == "punctuation" && text(*n, src) == "=")
        .and_then(|eq| children.get(eq + 1))?;
    let (name, span) = name_value(*value, src)?;
    Some(ContextInstance {
        name,
        type_name,
        span,
    })
}

fn named_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.start_byte()..node.end_byte()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document;

    fn contexts(src: &str) -> Vec<ContextType> {
        read(&document::parse(src, None), src)
    }

    #[test]
    fn reads_a_bare_name() {
        // `ly/engraver-init.ly` writes every `\name` this way.
        let src = "\\context { \\name Staff }";
        let types = contexts(src);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name, "Staff");
        let span = types[0].name_span;
        assert_eq!(&src[span.start..span.end], "Staff");
    }

    #[test]
    fn reads_a_quoted_name() {
        let src = "\\context { \\name \"MyStaff\" }";
        let types = contexts(src);
        assert_eq!(types[0].name, "MyStaff");
        // The span covers the contents, not the surrounding quotes.
        let span = types[0].name_span;
        assert_eq!(&src[span.start..span.end], "MyStaff");
    }

    #[test]
    fn reads_several_aliases() {
        let src = "\\context { \\name Dynamics \\alias Voice \\alias Staff }";
        assert_eq!(
            contexts(src)[0].aliases,
            vec!["Voice".to_string(), "Staff".to_string()]
        );
    }

    #[test]
    fn reads_a_description_through_texinfo_conversion() {
        let src = "\\context { \\name Staff \\description \"Handles @code{clef}s.\" }";
        assert_eq!(
            contexts(src)[0].description,
            Some("Handles `clef`s.".to_string())
        );
    }

    #[test]
    fn a_context_with_no_name_declares_no_type() {
        // `\context { \Staff \override … }` modifies an existing type; there
        // is nothing here for go-to-definition to land on.
        let src = "\\context { \\Staff \\override NoteHead.color = #red }";
        assert!(contexts(src).is_empty());
    }

    #[test]
    fn a_new_context_invocation_declares_no_type() {
        // `\new Staff = "x"` parses as `named_context`, an entirely different
        // shape from a `\context { … }` declaration block — this must not be
        // mistaken for one just because it also mentions `Staff`.
        for src in [
            "\\new Staff = \"x\" { c4 }",
            "\\context Staff = \"x\" { c4 }",
        ] {
            assert!(contexts(src).is_empty(), "wrongly read a type from {src}");
        }
    }

    #[test]
    fn a_block_that_inherits_before_naming_is_still_a_declaration() {
        // The canonical way to declare a context type: take an existing one
        // wholesale and rename it. `\Staff` takes no value, so a reader that
        // assumes every directive is followed by one falls out of step and
        // misses the `\name` that follows.
        let src = "\\context { \\Staff \\name MyStaff \\alias Staff }";
        let types = contexts(src);
        assert_eq!(types.len(), 1, "an inheriting block still declares a type");
        assert_eq!(types[0].name, "MyStaff");
        assert_eq!(types[0].aliases, vec!["Staff".to_string()]);
    }

    #[test]
    fn several_blocks_are_read_in_source_order() {
        let src = "\\layout { \\context { \\name Staff } \\context { \\name Voice } }";
        let names: Vec<String> = contexts(src).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["Staff".to_string(), "Voice".to_string()]);
    }

    #[test]
    fn a_block_nested_two_levels_deep_is_found() {
        // Inside `\layout` inside `\score`, the shape every real file uses.
        let src = "\\score { { c4 } \\layout { \\context { \\name Staff } } }";
        assert_eq!(contexts(src)[0].name, "Staff");
    }

    #[test]
    fn only_a_name_inside_the_matching_block_counts() {
        // A `\name` in a sibling block must not be attributed to this one.
        let src = "\\layout { \\context { \\Staff } \\context { \\name Voice } }";
        let types = contexts(src);
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name, "Voice");
    }

    #[test]
    fn block_span_covers_the_keyword_through_the_closing_brace() {
        let src = "x = { c } \\context { \\name Staff }";
        let span = contexts(src)[0].block_span;
        assert_eq!(&src[span.start..span.end], "\\context { \\name Staff }");
    }

    #[test]
    fn a_description_with_an_escaped_quote_and_a_backslashed_command_survives() {
        // Real descriptions in `engraver-init.ly` run to several lines and
        // reference commands with `@code{\clef}`; the escape-splitting the
        // grammar does inside a string must not truncate either.
        let src =
            "\\context { \\name Staff \\description \"A @code{\\\\clef} and a \\\"quote\\\".\" }";
        assert_eq!(
            contexts(src)[0].description,
            Some("A `\\clef` and a \"quote\".".to_string())
        );
    }

    fn instances(src: &str) -> Vec<ContextInstance> {
        read_instances(&document::parse(src, None), src)
    }

    #[test]
    fn new_reads_a_quoted_instance_name() {
        let src = "{ \\new Voice = \"vocals\" { c } }";
        let found = instances(src);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "vocals");
        assert_eq!(found[0].type_name, Some("Voice".to_string()));
        let span = found[0].span;
        assert_eq!(&src[span.start..span.end], "vocals");
    }

    #[test]
    fn context_reads_a_quoted_instance_name() {
        let src = "{ \\context Voice = \"vocals\" { c } }";
        let found = instances(src);
        assert_eq!(found[0].name, "vocals");
        assert_eq!(found[0].type_name, Some("Voice".to_string()));
    }

    #[test]
    fn a_bare_symbol_name_is_read_the_same_as_a_quoted_one() {
        // LilyPond accepts `\new Voice = vocals` unquoted just as readily.
        let src = "{ \\new Voice = vocals { c } }";
        let found = instances(src);
        assert_eq!(found[0].name, "vocals");
        let span = found[0].span;
        assert_eq!(&src[span.start..span.end], "vocals");
    }

    #[test]
    fn an_unnamed_new_declares_no_instance() {
        // `\new Voice { c }` creates a context but never names it — nothing
        // for go-to-definition to land on.
        let src = "{ \\new Voice { c } }";
        assert!(instances(src).is_empty());
    }

    #[test]
    fn change_refers_to_an_instance_rather_than_creating_one() {
        // `\change` moves the music that follows into an already-existing
        // context; see the reasoning on `read_instances`. It parses as a
        // flat run of siblings, never a `named_context`, so this reader
        // (which matches only `named_context` nodes) sees nothing here at
        // all — confirming the grammar shape the design relies on.
        let src = "{ \\change Staff = \"lower\" }";
        assert!(instances(src).is_empty());
    }

    #[test]
    fn lyricsto_names_no_instance_either() {
        // `\lyricsto`'s voice name is a reference too, and — like `\change`
        // — never sits inside a `named_context` node.
        let src = "{ \\lyricsto \"vocals\" { la la } }";
        assert!(instances(src).is_empty());
    }

    #[test]
    fn several_instances_are_read_in_source_order() {
        let src = "{ \\new Staff = \"upper\" { c } \\new Staff = \"lower\" { c } }";
        let names: Vec<String> = instances(src).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["upper".to_string(), "lower".to_string()]);
    }

    #[test]
    fn a_with_block_between_the_name_and_the_body_doesnt_confuse_the_reader() {
        // The `\with { … }` block is a sibling after the `named_context`
        // node, not one of its children, so it plays no part in this read at
        // all.
        let src = "{ \\new Voice = \"vocals\" \\with { fontSize = #-2 } { c } }";
        let found = instances(src);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "vocals");
    }
}
