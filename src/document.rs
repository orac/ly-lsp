//! Per-document analysis: parses source with the tree-sitter LilyPond grammar,
//! extracts the symbols (definitions and references) we answer
//! go-to-definition and find-references with, and converts between byte offsets
//! and LSP positions.

use std::sync::OnceLock;

use streaming_iterator::StreamingIterator;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, Position, Range, TextDocumentContentChangeEvent,
};
use tree_sitter::{InputEdit, Language, Node, Parser, Point, Query, QueryCursor, Tree};

use crate::line_struct::{LineIndex, Span};
use crate::note_analyser;
use crate::notes::{Events, NoteAnalysis, Problem};

/// A named occurrence in the source: either a definition (`foo = ...`) or a
/// reference (`\foo`). `span` covers the clickable extent of the occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub span: Span,
}

/// An `\include "path"` directive. `path` is the (possibly relative) string
/// given in the source; `span` covers the quoted string for navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Include {
    pub path: String,
    pub span: Span,
}

/// The outcome of analysing a parse tree.
struct Analysis {
    definitions: Vec<Symbol>,
    references: Vec<Symbol>,
    includes: Vec<Include>,
}

/// An analysed document. Holds the source text and its parse tree (so edits
/// can be reparsed incrementally), a line index for position conversion, and
/// the symbols found within.
#[derive(Debug)]
pub struct Document {
    text: String,
    tree: Tree,
    line_index: LineIndex,
    definitions: Vec<Symbol>,
    references: Vec<Symbol>,
    includes: Vec<Include>,
    notes: NoteAnalysis,
}

impl Document {
    pub fn new(text: String) -> Self {
        let tree = parse(&text, None);
        Self::from_parts(text, tree)
    }

    /// Builds the derived state (line index and symbols) for `text` and `tree`.
    fn from_parts(text: String, tree: Tree) -> Self {
        let line_index = LineIndex::new(&text);
        let analysis = extract(&tree, &text);
        let notes = note_analyser::analyse(&tree, &text);
        Self {
            text,
            tree,
            line_index,
            definitions: analysis.definitions,
            references: analysis.references,
            includes: analysis.includes,
            notes,
        }
    }

    /// Applies a single LSP content change. A change with a `range` is spliced
    /// into the text and reparsed incrementally against the old tree; a change
    /// without one replaces the whole document.
    pub fn apply_change(&mut self, change: TextDocumentContentChangeEvent) {
        let Some(range) = change.range else {
            *self = Document::new(change.text);
            return;
        };

        let start_byte = self.line_index.offset_clamped(range.start);
        let old_end_byte = self.line_index.offset_clamped(range.end);

        // tree-sitter points use byte columns; compute the start and old-end
        // points from the text as it stands before the splice.
        let start_position = point_at(&self.text, start_byte);
        let old_end_position = point_at(&self.text, old_end_byte);

        self.text
            .replace_range(start_byte..old_end_byte, &change.text);
        let new_end_byte = start_byte + change.text.len();
        let new_end_position = point_at(&self.text, new_end_byte);

        self.tree.edit(&InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position,
            old_end_position,
            new_end_position,
        });

        let tree = parse(&self.text, Some(&self.tree));
        *self = Self::from_parts(std::mem::take(&mut self.text), tree);
    }

    /// The document's source text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The line index, for converting between byte offsets and LSP positions.
    pub(crate) fn line_index(&self) -> &LineIndex {
        &self.line_index
    }

    /// The root of the parse tree.
    pub(crate) fn root_node(&self) -> Node<'_> {
        self.tree.root_node()
    }

    pub fn definitions(&self) -> &[Symbol] {
        &self.definitions
    }

    pub fn references(&self) -> &[Symbol] {
        &self.references
    }

    pub fn includes(&self) -> &[Include] {
        &self.includes
    }

    /// The lexically resolved music events, queryable by position or span.
    pub fn notes(&self) -> &Events {
        &self.notes.events
    }

    /// Syntax diagnostics from the parse tree. Tree-sitter's error recovery
    /// marks regions it couldn't parse as ERROR nodes and tokens it expected
    /// but didn't find as MISSING nodes; we surface both.
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let root = self.tree.root_node();
        if root.has_error() {
            self.collect_syntax_errors(root, &mut diagnostics);
        }
        for &problem in &self.notes.problems {
            let span = problem.span();
            let (severity, message) = match problem {
                Problem::NotANote(_) => (
                    DiagnosticSeverity::WARNING,
                    format!("`{}` is not a note name", &self.text[span.start..span.end]),
                ),
                Problem::DanglingColon(_) => (
                    DiagnosticSeverity::ERROR,
                    "expected a chord modifier or tremolo after `:`".to_string(),
                ),
            };
            diagnostics.push(Diagnostic {
                range: self.line_index.range_of(span),
                severity: Some(severity),
                source: Some("ly-lsp".to_string()),
                message,
                ..Diagnostic::default()
            });
        }
        diagnostics
    }

    /// A diagnostic for every `\foo` reference whose name `is_known` rejects.
    ///
    /// The caller supplies the notion of "known" — typically a built-in command
    /// or a definition reachable through includes — so this stays oblivious to
    /// both the builtin list and the include graph.
    pub fn undefined_reference_diagnostics(
        &self,
        is_known: impl Fn(&str) -> bool,
    ) -> Vec<Diagnostic> {
        self.references
            .iter()
            .filter(|reference| !is_known(&reference.name))
            .map(|reference| Diagnostic {
                range: self.line_index.range_of(reference.span),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("ly-lsp".to_string()),
                message: format!("undefined reference to `\\{}`", reference.name),
                ..Diagnostic::default()
            })
            .collect()
    }

    /// Walks `node`, pushing a diagnostic for each ERROR and MISSING node and
    /// pruning subtrees that contain no errors.
    fn collect_syntax_errors(&self, node: Node, out: &mut Vec<Diagnostic>) {
        if node.is_missing() {
            out.push(self.syntax_error(node, format!("missing `{}`", node.kind())));
            return;
        }
        if node.is_error() {
            // Tree-sitter wraps a region it couldn't parse in a single ERROR
            // node, which on its own only tells us "something is wrong here".
            // An unbalanced bracket or quote is by far the most common cause,
            // so look for the offending delimiter and point at it directly;
            // fall back to the generic message when nothing stands out.
            let unbalanced = self.unbalanced_delimiters(node);
            if unbalanced.is_empty() {
                out.push(self.syntax_error(node, "syntax error".to_string()));
            } else {
                out.extend(unbalanced);
            }
            return;
        }
        if !node.has_error() {
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect_syntax_errors(child, out);
        }
    }

    /// Pairs off the bracket and quote tokens directly under an ERROR node and
    /// returns a diagnostic for each one left unmatched. Well-formed nested
    /// structures are folded into named children, so the loose delimiter tokens
    /// we see here are exactly the ones the parser couldn't balance.
    fn unbalanced_delimiters(&self, node: Node) -> Vec<Diagnostic> {
        let mut pending: Vec<Node> = Vec::new();
        let mut diagnostics = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "{" | "<<" | "<" => pending.push(child),
                "}" | ">>" | ">" => {
                    if pending
                        .last()
                        .is_some_and(|open| closes(open.kind(), child.kind()))
                    {
                        pending.pop();
                    } else {
                        diagnostics.push(
                            self.syntax_error(child, format!("unmatched `{}`", child.kind())),
                        );
                    }
                }
                // A quote is its own closer: it closes a string already open,
                // otherwise it opens one.
                "\"" => {
                    if pending.last().is_some_and(|open| open.kind() == "\"") {
                        pending.pop();
                    } else {
                        pending.push(child);
                    }
                }
                _ => {}
            }
        }
        for open in pending {
            diagnostics.push(self.syntax_error(open, format!("unclosed `{}`", open.kind())));
        }
        diagnostics
    }

    fn syntax_error(&self, node: Node, message: String) -> Diagnostic {
        let span = Span::new(node.start_byte(), node.end_byte());
        Diagnostic {
            range: self.line_index.range_of(span),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ly-lsp".to_string()),
            message,
            ..Diagnostic::default()
        }
    }

    /// Returns the include path under `position`, if the cursor is on one.
    pub fn include_at(&self, position: Position) -> Option<&str> {
        let offset = self.line_index.offset_at(position)?;
        self.includes
            .iter()
            .find(|i| i.span.contains(offset))
            .map(|i| i.path.as_str())
    }

    /// Returns the ranges of a matched bracket pair `[bracket, match]` if
    /// `position` is on a bracket token, or `None` if it is not.
    pub fn bracket_at(&self, position: Position) -> Option<[Range; 2]> {
        let offset = self.line_index.offset_at(position)?;
        let node = self
            .tree
            .root_node()
            .descendant_for_byte_range(offset, offset)?;

        let target_kind = match node.kind() {
            "{" => "}",
            "}" => "{",
            "<<" => ">>",
            ">>" => "<<",
            "<" => ">",
            ">" => "<",
            _ => return None,
        };
        let is_open = matches!(node.kind(), "{" | "<<" | "<");

        let node_span = Span::new(node.start_byte(), node.end_byte());
        let parent = node.parent()?;
        let mut cursor = parent.walk();
        let children: Vec<_> = parent.children(&mut cursor).collect();

        let match_node = if is_open {
            children
                .into_iter()
                .find(|c| c.kind() == target_kind && c.start_byte() > node.start_byte())
        } else {
            children
                .into_iter()
                .rfind(|c| c.kind() == target_kind && c.start_byte() < node.start_byte())
        }?;

        Some([
            self.line_index.range_of(node_span),
            self.line_index
                .range_of(Span::new(match_node.start_byte(), match_node.end_byte())),
        ])
    }

    /// Returns the name of the definition or reference under `position`, if any.
    pub fn symbol_at(&self, position: Position) -> Option<&str> {
        let offset = self.line_index.offset_at(position)?;
        self.definitions
            .iter()
            .chain(&self.references)
            .find(|s| s.span.contains(offset))
            .map(|s| s.name.as_str())
    }

    /// Definitions matching `name`, as LSP ranges.
    pub fn definition_ranges(&self, name: &str) -> Vec<Range> {
        self.ranges(&self.definitions, name)
    }

    /// References matching `name`, as LSP ranges.
    pub fn reference_ranges(&self, name: &str) -> Vec<Range> {
        self.ranges(&self.references, name)
    }

    fn ranges(&self, symbols: &[Symbol], name: &str) -> Vec<Range> {
        symbols
            .iter()
            .filter(|s| s.name == name)
            .map(|s| self.line_index.range_of(s.span))
            .collect()
    }
}

/// The LilyPond grammar as a tree-sitter [`Language`].
fn language() -> Language {
    tree_sitter_lilypond::LANGUAGE_LILYPOND.into()
}

/// Query capturing the symbols we care about.
///
/// A `@definition` is the name `symbol` of a top-level assignment (a direct
/// child of `lilypond_program`), which excludes header fields, `\with` blocks
/// and `\override` property paths. A `@reference` is any `\foo` command; most
/// resolve to no definition because they're built-ins, which is exactly why
/// go-to-definition leaves them alone.
const SYMBOL_QUERY: &str = r#"
(lilypond_program (assignment_lhs (symbol) @definition))
(escaped_word) @reference
"#;

fn symbol_query() -> &'static Query {
    static QUERY: OnceLock<Query> = OnceLock::new();
    QUERY.get_or_init(|| Query::new(&language(), SYMBOL_QUERY).expect("valid query"))
}

/// Parses `src`, reusing `old_tree` for incremental reparsing when supplied.
fn parse(src: &str, old_tree: Option<&Tree>) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&language())
        .expect("load LilyPond grammar");
    parser
        .parse(src, old_tree)
        .expect("parser produces a tree for any input")
}

/// Extracts definitions, references and includes from `tree`.
///
/// `\include` is captured as an `escaped_word` like any other command, then
/// recognised here by its text and paired with the string literal that follows
/// it, so it becomes an [`Include`] rather than a (meaningless) reference.
fn extract(tree: &Tree, src: &str) -> Analysis {
    let query = symbol_query();
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut definitions = Vec::new();
    let mut references = Vec::new();
    let mut includes = Vec::new();

    let mut matches = cursor.matches(query, tree.root_node(), src.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let node = cap.node;
            let span = Span::new(node.start_byte(), node.end_byte());
            let text = &src[span.start..span.end];
            match capture_names[cap.index as usize] {
                "definition" => definitions.push(Symbol {
                    name: text.to_string(),
                    span,
                }),
                "reference" if text == "\\include" => {
                    includes.extend(include_after(node, src));
                }
                "reference" => references.push(Symbol {
                    // Strip the leading backslash from `\foo`.
                    name: text[1..].to_string(),
                    span,
                }),
                _ => {}
            }
        }
    }

    Analysis {
        definitions,
        references,
        includes,
    }
}

/// Reads the `\include` target from the string literal following the `\include`
/// keyword node, if present.
fn include_after(keyword: Node, src: &str) -> Option<Include> {
    let string_node = keyword.next_named_sibling()?;
    if string_node.kind() != "string" {
        return None;
    }
    let mut cursor = string_node.walk();
    let fragment = string_node
        .named_children(&mut cursor)
        .find(|n| n.kind() == "string_fragment")?;
    Some(Include {
        path: src[fragment.start_byte()..fragment.end_byte()].to_string(),
        // The span covers the whole quoted string so clicking the path works.
        span: Span::new(string_node.start_byte(), string_node.end_byte()),
    })
}

/// Computes the tree-sitter [`Point`] (row, byte-column) for a byte offset into
/// `text`. Unlike LSP positions, tree-sitter columns are counted in bytes.
fn point_at(text: &str, byte: usize) -> Point {
    let before = &text[..byte];
    let row = before.bytes().filter(|&b| b == b'\n').count();
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Point::new(row, byte - line_start)
}

/// Whether `close` is the closing delimiter matching the opener `open`.
fn closes(open: &str, close: &str) -> bool {
    matches!((open, close), ("{", "}") | ("<<", ">>") | ("<", ">"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(symbols: &[Symbol]) -> Vec<&str> {
        symbols.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn finds_top_level_definition() {
        let doc = Document::new("foo = { c d e }".to_string());
        assert_eq!(names(doc.definitions()), vec!["foo"]);
    }

    #[test]
    fn finds_reference() {
        let doc = Document::new("\\foo".to_string());
        assert_eq!(names(doc.references()), vec!["foo"]);
    }

    #[test]
    fn header_field_is_not_a_definition() {
        // `title` is a header field nested in a block, not a top-level variable.
        let doc = Document::new("\\header { title = \"X\" }".to_string());
        assert!(doc.definitions().is_empty());
    }

    #[test]
    fn override_property_is_not_a_definition() {
        let doc = Document::new("\\score { \\override NoteHead.color = #red }".to_string());
        assert!(doc.definitions().is_empty());
    }

    #[test]
    fn builtins_are_references_without_definitions() {
        let doc = Document::new("\\relative c' { c d e }".to_string());
        assert_eq!(names(doc.references()), vec!["relative"]);
        assert!(doc.definition_ranges("relative").is_empty());
    }

    #[test]
    fn comment_contents_are_ignored() {
        // Neither the assignment nor the command inside a comment count.
        let doc = Document::new("% foo = \\bar\nbaz = { c }\n".to_string());
        assert_eq!(names(doc.definitions()), vec!["baz"]);
        assert!(doc.references().is_empty());
    }

    #[test]
    fn string_contents_are_ignored() {
        let doc = Document::new("title = \"a \\foo b\"".to_string());
        assert!(doc.references().is_empty());
    }

    #[test]
    fn symbol_at_definition_and_reference() {
        let src = "foo = { c }\n\\foo\n";
        let doc = Document::new(src.to_string());
        // On the `foo` of the definition (line 0, char 1).
        assert_eq!(doc.symbol_at(Position::new(0, 1)), Some("foo"));
        // On the `\foo` reference (line 1, char 2, i.e. the first `o`).
        assert_eq!(doc.symbol_at(Position::new(1, 2)), Some("foo"));
        // On whitespace.
        assert_eq!(doc.symbol_at(Position::new(0, 4)), None);
    }

    #[test]
    fn definition_and_reference_ranges() {
        let src = "foo = { c }\n\\foo\n";
        let doc = Document::new(src.to_string());
        let defs = doc.definition_ranges("foo");
        assert_eq!(
            defs,
            vec![Range::new(Position::new(0, 0), Position::new(0, 3))]
        );
        let refs = doc.reference_ranges("foo");
        assert_eq!(
            refs,
            vec![Range::new(Position::new(1, 0), Position::new(1, 4))]
        );
    }

    #[test]
    fn utf16_positions_account_for_wide_characters() {
        // `café` then a definition; the é is one UTF-16 unit but two bytes.
        // A musical 𝄞 (U+1D11E) is two UTF-16 units and four bytes.
        let src = "% café 𝄞\nfoo = 1\n";
        let doc = Document::new(src.to_string());
        let defs = doc.definition_ranges("foo");
        assert_eq!(
            defs,
            vec![Range::new(Position::new(1, 0), Position::new(1, 3))]
        );

        // Round-trip a position on the second line.
        let index = LineIndex::new(src);
        let pos = Position::new(1, 0);
        let offset = index.offset_at(pos).unwrap();
        assert_eq!(index.position_at(offset), pos);
    }

    #[test]
    fn multiple_references_to_same_variable() {
        let src = "foo = { c }\n\\foo \\foo\n";
        let doc = Document::new(src.to_string());
        assert_eq!(doc.reference_ranges("foo").len(), 2);
    }

    /// Builds a ranged content change replacing `range` with `text`.
    fn change(range: Range, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(range),
            range_length: None,
            text: text.to_string(),
        }
    }

    #[test]
    fn incremental_edit_renames_definition() {
        let mut doc = Document::new("foo = { c }\n\\foo\n".to_string());
        // Replace the `foo` on line 0 (chars 0..3) with `bar`.
        doc.apply_change(change(
            Range::new(Position::new(0, 0), Position::new(0, 3)),
            "bar",
        ));

        assert_eq!(names(doc.definitions()), vec!["bar"]);
        assert_eq!(doc.definition_ranges("bar").len(), 1);
        assert!(doc.definition_ranges("foo").is_empty());
        // The reference is unchanged and now dangles.
        assert_eq!(names(doc.references()), vec!["foo"]);
    }

    #[test]
    fn incremental_insert_shifts_later_positions() {
        let mut doc = Document::new("foo = { c }\n\\foo\n".to_string());
        // Insert a new top-level definition and a blank line at the very start.
        doc.apply_change(change(
            Range::new(Position::new(0, 0), Position::new(0, 0)),
            "bar = { d }\n",
        ));

        assert_eq!(names(doc.definitions()), vec!["bar", "foo"]);
        // `foo`'s definition has moved from line 0 to line 1.
        assert_eq!(
            doc.definition_ranges("foo"),
            vec![Range::new(Position::new(1, 0), Position::new(1, 3))]
        );
        // The `\foo` reference has shifted down to line 2.
        assert_eq!(
            doc.reference_ranges("foo"),
            vec![Range::new(Position::new(2, 0), Position::new(2, 4))]
        );
    }

    #[test]
    fn full_replacement_change_replaces_document() {
        let mut doc = Document::new("foo = { c }\n".to_string());
        // A change with no range carries the entire new document.
        doc.apply_change(TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "baz = { e }\n\\baz\n".to_string(),
        });

        assert_eq!(names(doc.definitions()), vec!["baz"]);
        assert_eq!(names(doc.references()), vec!["baz"]);
        assert!(doc.definition_ranges("foo").is_empty());
    }

    #[test]
    fn well_formed_source_has_no_diagnostics() {
        let doc = Document::new("foo = { c d e }\n\\foo\n".to_string());
        assert!(doc.diagnostics().is_empty());
    }

    #[test]
    fn unclosed_brace_is_reported_at_the_brace() {
        let doc = Document::new("foo = { c d e\n".to_string());
        let diagnostics = doc.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostics[0].message, "unclosed `{`");
        // Localised to the `{` itself, not the whole region after it.
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(0, 6), Position::new(0, 7))
        );
    }

    #[test]
    fn unclosed_quote_is_reported_at_the_quote() {
        let doc = Document::new("foo = { c \"hello\n".to_string());
        let messages: Vec<String> = doc.diagnostics().into_iter().map(|d| d.message).collect();
        assert!(
            messages.iter().any(|m| m == "unclosed `\"`"),
            "expected an unclosed-quote diagnostic, got {messages:?}"
        );
    }

    #[test]
    fn unmatched_close_brace_is_reported_at_the_brace() {
        let doc = Document::new("foo = { c d } }\n".to_string());
        let diagnostics = doc.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "unmatched `}`");
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(0, 14), Position::new(0, 15))
        );
    }

    #[test]
    fn parses_include_directives() {
        let doc =
            Document::new("\\include \"notes.ily\"\n\\include \"parts/violin.ily\"\n".to_string());
        let paths: Vec<&str> = doc.includes().iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, vec!["notes.ily", "parts/violin.ily"]);
        // `\include` itself is not a reference.
        assert!(doc.references().is_empty());
    }

    #[test]
    fn include_at_finds_path_under_cursor() {
        let doc = Document::new("\\include \"notes.ily\"\n".to_string());
        // Cursor inside the quoted path (char 12 is within "notes.ily").
        assert_eq!(doc.include_at(Position::new(0, 12)), Some("notes.ily"));
        // Cursor on the `\include` keyword resolves to no include.
        assert_eq!(doc.include_at(Position::new(0, 2)), None);
    }

    #[test]
    fn bracket_at_sequential_music() {
        let doc = Document::new("foo = { c d e }\n".to_string());
        // Cursor on `{` (char 6).
        let [open, close] = doc.bracket_at(Position::new(0, 6)).expect("bracket pair");
        assert_eq!(open, Range::new(Position::new(0, 6), Position::new(0, 7)));
        assert_eq!(
            close,
            Range::new(Position::new(0, 14), Position::new(0, 15))
        );
        // Cursor on `}` (char 14) — same pair, reversed.
        let [close2, open2] = doc.bracket_at(Position::new(0, 14)).expect("bracket pair");
        assert_eq!(
            close2,
            Range::new(Position::new(0, 14), Position::new(0, 15))
        );
        assert_eq!(open2, Range::new(Position::new(0, 6), Position::new(0, 7)));
    }

    #[test]
    fn bracket_at_simultaneous_music() {
        let doc = Document::new("<< { c } { d } >>\n".to_string());
        // Cursor on first `<` of `<<` (char 0).
        let [open, close] = doc.bracket_at(Position::new(0, 0)).expect("bracket pair");
        assert_eq!(open, Range::new(Position::new(0, 0), Position::new(0, 2)));
        assert_eq!(
            close,
            Range::new(Position::new(0, 15), Position::new(0, 17))
        );
    }

    #[test]
    fn bracket_at_chord() {
        let doc = Document::new("{ <c e g> }\n".to_string());
        // Cursor on `<` (char 2).
        let [open, close] = doc
            .bracket_at(Position::new(0, 2))
            .expect("chord bracket pair");
        assert_eq!(open, Range::new(Position::new(0, 2), Position::new(0, 3)));
        assert_eq!(close, Range::new(Position::new(0, 8), Position::new(0, 9)));
    }

    #[test]
    fn bracket_at_note_returns_none() {
        let doc = Document::new("{ c d e }\n".to_string());
        // Cursor on `c` (a note, char 2) — not a bracket.
        assert!(doc.bracket_at(Position::new(0, 2)).is_none());
    }

    #[test]
    fn edit_into_a_comment_drops_the_definition() {
        // Commenting out a definition by prefixing the line with `% ` should
        // make it disappear after reparsing.
        let mut doc = Document::new("foo = { c }\n".to_string());
        doc.apply_change(change(
            Range::new(Position::new(0, 0), Position::new(0, 0)),
            "% ",
        ));
        assert!(doc.definitions().is_empty());
    }
}
