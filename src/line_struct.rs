use tower_lsp::lsp_types::{Position, Range};

/// A half-open byte range `[start, end)` into the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn contains(&self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

/// Maps byte offsets to LSP positions and back. LSP positions count UTF-16
/// code units within a line, so we convert carefully rather than assuming
/// ASCII.
#[derive(Debug)]
pub struct LineIndex {
    text: String,
    /// Byte offset of the start of each line.
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
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
    pub fn position_at(&self, offset: usize) -> Position {
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
    pub fn offset_at(&self, position: Position) -> Option<usize> {
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
    pub fn offset_clamped(&self, position: Position) -> usize {
        self.offset_at(position).unwrap_or(self.text.len())
    }

    pub fn range_of(&self, span: Span) -> Range {
        Range::new(self.position_at(span.start), self.position_at(span.end))
    }
}
