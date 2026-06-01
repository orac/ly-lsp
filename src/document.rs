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

/// A half-open byte range `[start, end)` into the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    fn contains(&self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

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
        Self {
            text,
            tree,
            line_index,
            definitions: analysis.definitions,
            references: analysis.references,
            includes: analysis.includes,
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

        self.text.replace_range(start_byte..old_end_byte, &change.text);
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

    pub fn definitions(&self) -> &[Symbol] {
        &self.definitions
    }

    pub fn references(&self) -> &[Symbol] {
        &self.references
    }

    pub fn includes(&self) -> &[Include] {
        &self.includes
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
            out.push(self.syntax_error(node, "syntax error".to_string()));
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

/// Maps byte offsets to LSP positions and back. LSP positions count UTF-16
/// code units within a line, so we convert carefully rather than assuming
/// ASCII.
#[derive(Debug)]
struct LineIndex {
    text: String,
    /// Byte offset of the start of each line.
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        for (offset, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(offset + 1);
            }
        }
        Self {
            text: text.to_string(),
            line_starts,
        }
    }

    /// Converts a byte offset to an LSP position.
    fn position_at(&self, offset: usize) -> Position {
        // The line is the last line start that is <= offset.
        let line = match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next - 1,
        };
        let line_start = self.line_starts[line];
        let character = self.text[line_start..offset].encode_utf16().count() as u32;
        Position::new(line as u32, character)
    }

    /// Converts an LSP position to a byte offset, or `None` if it falls outside
    /// the text.
    fn offset_at(&self, position: Position) -> Option<usize> {
        let line_start = *self.line_starts.get(position.line as usize)?;
        let line_end = self
            .line_starts
            .get(position.line as usize + 1)
            .copied()
            .unwrap_or(self.text.len());

        // Walk the line, counting UTF-16 units, to find the byte offset for the
        // requested character.
        let mut utf16 = 0u32;
        for (byte_offset, ch) in self.text[line_start..line_end].char_indices() {
            if utf16 >= position.character {
                return Some(line_start + byte_offset);
            }
            utf16 += ch.len_utf16() as u32;
        }
        // Position is at (or past) the end of the line; clamp to line end.
        Some(line_end)
    }

    /// Like [`offset_at`](Self::offset_at) but clamps an out-of-range position
    /// to the end of the document, so applying an edit can't fail.
    fn offset_clamped(&self, position: Position) -> usize {
        self.offset_at(position).unwrap_or(self.text.len())
    }

    fn range_of(&self, span: Span) -> Range {
        Range::new(self.position_at(span.start), self.position_at(span.end))
    }
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
        doc.apply_change(change(Range::new(Position::new(0, 0), Position::new(0, 3)), "bar"));

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
    fn unclosed_brace_is_reported() {
        let doc = Document::new("foo = { c d e\n".to_string());
        let diagnostics = doc.diagnostics();
        assert!(
            !diagnostics.is_empty(),
            "expected a diagnostic for the unclosed brace"
        );
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn parses_include_directives() {
        let doc = Document::new("\\include \"notes.ily\"\n\\include \"parts/violin.ily\"\n".to_string());
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
    fn edit_into_a_comment_drops_the_definition() {
        // Commenting out a definition by prefixing the line with `% ` should
        // make it disappear after reparsing.
        let mut doc = Document::new("foo = { c }\n".to_string());
        doc.apply_change(change(Range::new(Position::new(0, 0), Position::new(0, 0)), "% "));
        assert!(doc.definitions().is_empty());
    }
}
