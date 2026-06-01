//! Per-document analysis: turns source text into the symbols (definitions and
//! references) we can answer go-to-definition and find-references with, and
//! converts between byte offsets and LSP positions.

use tower_lsp::lsp_types::{Position, Range};

use crate::lexer::{tokenise, Span, TokenKind};

/// A named occurrence in the source: either a definition (`foo = ...`) or a
/// reference (`\foo`). `span` covers the clickable extent of the occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub span: Span,
}

/// An analysed document. Holds the source, a line index for position
/// conversion, and the symbols found within.
#[derive(Debug)]
pub struct Document {
    line_index: LineIndex,
    definitions: Vec<Symbol>,
    references: Vec<Symbol>,
}

impl Document {
    pub fn new(text: String) -> Self {
        let line_index = LineIndex::new(&text);
        let (definitions, references) = analyse(&text);
        Self {
            line_index,
            definitions,
            references,
        }
    }

    pub fn definitions(&self) -> &[Symbol] {
        &self.definitions
    }

    pub fn references(&self) -> &[Symbol] {
        &self.references
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

/// Walks the token stream to collect definitions and references.
///
/// A definition is an identifier immediately followed by `=` at brace-depth 0;
/// this keeps property assignments inside `\paper`, `\with`, `\header` blocks
/// and the like from masquerading as top-level variables. A reference is any
/// `\foo` command — most will be built-ins with no matching definition, which
/// is exactly why go-to-definition on them resolves to nothing.
fn analyse(src: &str) -> (Vec<Symbol>, Vec<Symbol>) {
    let tokens = tokenise(src);
    let mut definitions = Vec::new();
    let mut references = Vec::new();
    let mut depth: i32 = 0;

    for (idx, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::OpenBrace => depth += 1,
            TokenKind::CloseBrace => depth = (depth - 1).max(0),
            TokenKind::Command => {
                // The reference name excludes the leading backslash.
                let text = &src[token.span.start..token.span.end];
                references.push(Symbol {
                    name: text[1..].to_string(),
                    span: token.span,
                });
            }
            TokenKind::Identifier if depth == 0 => {
                let followed_by_equals =
                    matches!(tokens.get(idx + 1).map(|t| t.kind), Some(TokenKind::Equals));
                if followed_by_equals {
                    definitions.push(Symbol {
                        name: src[token.span.start..token.span.end].to_string(),
                        span: token.span,
                    });
                }
            }
            _ => {}
        }
    }

    (definitions, references)
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
    fn nested_assignment_is_not_a_definition() {
        // `title` is a header field at depth 1, not a top-level variable.
        let doc = Document::new("\\header { title = \"X\" }".to_string());
        assert!(doc.definitions().is_empty());
    }

    #[test]
    fn builtins_are_references_without_definitions() {
        let doc = Document::new("\\relative c' { c d e }".to_string());
        assert_eq!(names(doc.references()), vec!["relative"]);
        assert!(doc.definition_ranges("relative").is_empty());
    }

    #[test]
    fn definition_then_reference() {
        let src = "foo = { c d e }\n\\foo\n";
        let doc = Document::new(src.to_string());
        assert_eq!(names(doc.definitions()), vec!["foo"]);
        assert_eq!(names(doc.references()), vec!["foo"]);
    }

    #[test]
    fn symbol_at_definition_and_reference() {
        let src = "foo = { c }\n\\foo\n";
        let doc = Document::new(src.to_string());
        // On the `foo` of the definition (line 0, char 1).
        assert_eq!(doc.symbol_at(Position::new(0, 1)), Some("foo"));
        // On the `\foo` reference (line 1, char 2, i.e. the `o`).
        assert_eq!(doc.symbol_at(Position::new(1, 2)), Some("foo"));
        // On whitespace.
        assert_eq!(doc.symbol_at(Position::new(0, 4)), None);
    }

    #[test]
    fn definition_and_reference_ranges() {
        let src = "foo = { c }\n\\foo\n";
        let doc = Document::new(src.to_string());
        let defs = doc.definition_ranges("foo");
        assert_eq!(defs, vec![Range::new(Position::new(0, 0), Position::new(0, 3))]);
        let refs = doc.reference_ranges("foo");
        assert_eq!(refs, vec![Range::new(Position::new(1, 0), Position::new(1, 4))]);
    }

    #[test]
    fn utf16_positions_account_for_wide_characters() {
        // `café` then a definition; the é is one UTF-16 unit but two bytes.
        // A musical 𝄞 (U+1D11E) is two UTF-16 units and four bytes.
        let src = "% café 𝄞\nfoo = 1\n";
        let doc = Document::new(src.to_string());
        let defs = doc.definition_ranges("foo");
        assert_eq!(defs, vec![Range::new(Position::new(1, 0), Position::new(1, 3))]);

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
}
