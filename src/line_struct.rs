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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Strings that mix ASCII, a two-byte character (`é`) and a four-byte one
    /// that needs a UTF-16 surrogate pair (the musical G clef, U+1D11E), plus
    /// newlines, so a generated case exercises every width `position_at` and
    /// `offset_at` have to convert between.
    fn text_strategy() -> impl Strategy<Value = String> {
        "[a-c\n\u{e9}\u{1D11E}]{0,40}"
    }

    /// Every byte offset the text actually has a character at, plus the
    /// offset one past the end — the full set of positions a caller could
    /// legitimately ask about.
    fn char_boundaries(text: &str) -> Vec<usize> {
        text.char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect()
    }

    proptest! {
        /// Converting a char boundary to a position and back must return the
        /// same offset: that round trip is the whole point of `LineIndex`,
        /// since the LSP client only ever speaks in positions.
        #[test]
        fn offset_at_undoes_position_at_for_every_char_boundary(text in text_strategy()) {
            let index = LineIndex::new(&text);
            for o in char_boundaries(&text) {
                prop_assert_eq!(index.offset_at(index.position_at(o)), Some(o));
            }
        }

        /// `position_at` must not reorder offsets: a later byte in the text
        /// can never map to an earlier (line, character) pair, or edits and
        /// cursor placement downstream would go backwards.
        #[test]
        fn position_at_is_monotonic_in_the_offset(text in text_strategy()) {
            let index = LineIndex::new(&text);
            let boundaries = char_boundaries(&text);
            for &a in &boundaries {
                for &b in &boundaries {
                    if a <= b {
                        let pa = index.position_at(a);
                        let pb = index.position_at(b);
                        prop_assert!((pa.line, pa.character) <= (pb.line, pb.character));
                    }
                }
            }
        }

        /// `offset_at` must only ever hand back an offset that sits on a char
        /// boundary, even for a `Position` that names a character past the
        /// end of its line or a line past the end of the document — a client
        /// can send either, and the result must still be safe to slice with.
        #[test]
        fn offset_at_never_returns_a_non_char_boundary(
            text in text_strategy(),
            line in 0u32..10,
            character in 0u32..80,
        ) {
            let index = LineIndex::new(&text);
            if let Some(offset) = index.offset_at(Position::new(line, character)) {
                prop_assert!(text.is_char_boundary(offset));
            }
        }

        /// `offset_clamped` exists precisely so a caller never has to handle
        /// a failed lookup: it must not panic, and whatever it returns must
        /// be a valid, in-bounds char boundary, however far outside the
        /// document the requested position lies.
        #[test]
        fn offset_clamped_never_panics_and_stays_in_bounds(
            text in text_strategy(),
            line in 0u32..10_000,
            character in 0u32..10_000,
        ) {
            let index = LineIndex::new(&text);
            let offset = index.offset_clamped(Position::new(line, character));
            prop_assert!(offset <= text.len());
            prop_assert!(text.is_char_boundary(offset));
        }

        /// The line component of a position is defined as a count of
        /// newlines, so `position_at` had better agree with counting them
        /// directly — otherwise `line_starts` and the newline scan it was
        /// built from have drifted apart.
        #[test]
        fn position_at_line_counts_preceding_newlines(text in text_strategy()) {
            let index = LineIndex::new(&text);
            for o in char_boundaries(&text) {
                let newlines_before = text.as_bytes()[..o].iter().filter(|&&b| b == b'\n').count();
                prop_assert_eq!(index.position_at(o).line as usize, newlines_before);
            }
        }
    }
}
