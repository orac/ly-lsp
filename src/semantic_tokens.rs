//! `textDocument/semanticTokens/full` support.
//!
//! The TextMate grammar highlights structurally, from token shape alone — it
//! cannot tell that the `volta` in `\repeat volta 2` is meaningful, because
//! that meaning comes from `\repeat`'s signature, not from anything `volta`
//! looks like on its own (it's the same `symbol` node as an arbitrary bare
//! word anywhere else). Semantic tokens fill exactly that gap: this module
//! walks a document's already-parsed [`Commands`] and emits one token per
//! argument whose meaning the grammar can't otherwise recover.
//!
//! # What gets a token, and what doesn't
//!
//! Only [`Arg::BareWord`] and [`Arg::Word`] arguments are emitted, both as
//! [`SemanticTokenType::KEYWORD`] — the two shapes doc/command-parsing.md
//! calls out as the reason semantic highlighting is worth having at all. The
//! other argument kinds were considered and left out:
//!
//! - [`Arg::String`]: a quoted `"bass"` is already a `string` node the
//!   grammar highlights directly; the bare-symbol form (`\clef bass`) is the
//!   same "can't tell it apart from an arbitrary symbol" problem as
//!   `BareWord`, but it names open-ended, command-specific text (clef names,
//!   language names) rather than a small closed vocabulary, so tagging it
//!   "keyword" would be a category error. Left out rather than invented a
//!   new type for it, on the YAGNI principle — nothing has asked for it yet.
//! - [`Arg::PropertyPath`]: `Staff.instrumentName` is already structurally
//!   distinct in the grammar (a dotted `property_expression`, not a bare
//!   `symbol`), so a TextMate rule can target it without needing command
//!   context in the first place. No gap to fill.
//! - [`Arg::Pitch`]: a reference pitch (`\relative c'`, the tonic of `\key c
//!   \major`) is written with exactly the same syntax as a note in the music
//!   body, and is already highlighted as one. Giving it a different semantic
//!   type would be a regression, not an improvement.
//!
//! # Encoding
//!
//! LSP semantic tokens are delta-encoded relative to the *previous* token
//! (line and start character are both deltas, never absolute) and must be
//! emitted in position order; see [`encode`]. Positions and lengths are UTF-16
//! code units, via [`LineIndex`], not bytes.
//!
//! [`Commands`]: crate::command::Commands

use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend,
};

use crate::command::Arg;
use crate::document::Document;
use crate::line_struct::{LineIndex, Span};

/// The token types this server emits, in legend order. [`token_type_index`]
/// looks a type's index up by searching this slice rather than a
/// hand-maintained constant, so adding a type here is the only change needed
/// to keep emission and the legend in agreement.
const TOKEN_TYPES: &[SemanticTokenType] = &[SemanticTokenType::KEYWORD];

/// No modifiers are used. Declared (empty) rather than omitted because some
/// clients expect the array to be present on the wire even when unused.
const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[];

/// The legend to declare in `ServerCapabilities`, matching [`TOKEN_TYPES`] and
/// [`TOKEN_MODIFIERS`] by construction.
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES.to_vec(),
        token_modifiers: TOKEN_MODIFIERS.to_vec(),
    }
}

/// `ty`'s index into [`TOKEN_TYPES`] — the numeric type code a
/// [`SemanticToken`] carries, resolved by position in the legend rather than
/// hard-coded, so a type this module emits and the legend it's declared
/// against can never drift apart.
fn token_type_index(ty: &SemanticTokenType) -> u32 {
    TOKEN_TYPES
        .iter()
        .position(|t| t == ty)
        .expect("emitted token type must be declared in TOKEN_TYPES") as u32
}

/// The semantic tokens for the whole of `doc`, delta-encoded and ready to
/// return from `textDocument/semanticTokens/full`. See the module docs for
/// which argument kinds are covered.
pub fn semantic_tokens_full(doc: &Document) -> Vec<SemanticToken> {
    let keyword = token_type_index(&SemanticTokenType::KEYWORD);
    let mut spans: Vec<Span> = doc
        .commands()
        .iter()
        .flat_map(|call| &call.args)
        .filter_map(|arg| match arg {
            Arg::BareWord { span, .. } | Arg::Word { span, .. } => Some(*span),
            _ => None,
        })
        .collect();
    // A call's own bare-word/word arguments always precede any call nested in
    // its body, so collecting in `Commands`' source order already yields
    // increasing spans; sorting here is a cheap belt-and-braces guard against
    // that invariant being wrong or changing under us, not load-bearing.
    spans.sort_by_key(|span| span.start);
    encode(doc.line_index(), &spans, keyword)
}

/// Delta-encodes `spans`, already sorted by start position, as tokens of
/// `token_type` with no modifiers. Each [`SemanticToken`] carries its
/// position as a *delta* from the previous token: `delta_line` relative to
/// the previous token's line, and `delta_start` relative to the previous
/// token's start character on the same line, or from the start of the line
/// otherwise. `lines` converts each span's byte offsets to UTF-16 positions,
/// since that's what LSP counts in, not bytes.
fn encode(lines: &LineIndex, spans: &[Span], token_type: u32) -> Vec<SemanticToken> {
    let mut tokens = Vec::with_capacity(spans.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for span in spans {
        let start = lines.position_at(span.start);
        let end = lines.position_at(span.end);
        debug_assert_eq!(
            start.line, end.line,
            "a bare-word/word argument never spans multiple lines"
        );
        let delta_line = start.line - prev_line;
        let delta_start = if delta_line == 0 {
            start.character - prev_start
        } else {
            start.character
        };
        tokens.push(SemanticToken {
            delta_line,
            delta_start,
            length: end.character - start.character,
            token_type,
            token_modifiers_bitset: 0,
        });
        prev_line = start.line;
        prev_start = start.character;
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_a_token_for_a_bare_word_argument() {
        let doc = Document::new("\\repeat volta 2 { c }".to_string());
        let tokens = semantic_tokens_full(&doc);
        assert_eq!(tokens.len(), 1);
        let token = tokens[0];
        assert_eq!(token.delta_line, 0);
        assert_eq!(token.delta_start, "\\repeat ".encode_utf16().count() as u32);
        assert_eq!(token.length, "volta".encode_utf16().count() as u32);
        assert_eq!(
            token.token_type,
            token_type_index(&SemanticTokenType::KEYWORD)
        );
    }

    #[test]
    fn emits_a_token_for_a_word_argument() {
        // `\major` is an `Arg::Word`, the escaped-word form.
        let doc = Document::new("\\key g \\major".to_string());
        let tokens = semantic_tokens_full(&doc);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].length, "\\major".encode_utf16().count() as u32);
    }

    #[test]
    fn emits_nothing_for_an_unrecognised_bare_word() {
        // Plain music has no command call at all, so nothing is tokenised.
        let doc = Document::new("c d e".to_string());
        assert!(semantic_tokens_full(&doc).is_empty());
    }

    #[test]
    fn deltas_are_relative_to_the_previous_token_not_absolute() {
        let src = "\\repeat volta 2 { c } \\repeat unfold 3 { d }";
        let doc = Document::new(src.to_string());
        let tokens = semantic_tokens_full(&doc);
        assert_eq!(tokens.len(), 2);

        // First token: absolute position, since there is no previous one.
        let first_start = src.find("volta").unwrap() as u32;
        assert_eq!(tokens[0].delta_start, first_start);

        // Second token: delta from the first token's *start*, not from its end,
        // and both are on the same line so delta_line is 0.
        let second_start = src.find("unfold").unwrap() as u32;
        assert_eq!(tokens[1].delta_line, 0);
        assert_eq!(tokens[1].delta_start, second_start - first_start);
    }

    #[test]
    fn a_multi_byte_character_earlier_on_the_line_shifts_utf16_offsets() {
        // "café " is 5 bytes for 4 chars ("é" is 2 bytes, 1 UTF-16 unit), so
        // the byte offset of "volta" and its UTF-16 character offset differ:
        // exercising this against LineIndex is the point of the test, not the
        // exact numbers, which is why they're derived rather than hard-coded.
        let src = "% café\n\\repeat volta 2 { c }";
        let doc = Document::new(src.to_string());
        let tokens = semantic_tokens_full(&doc);
        assert_eq!(tokens.len(), 1);

        let line_index = doc.line_index();
        let byte_offset = src.find("volta").unwrap();
        let expected = line_index.position_at(byte_offset);
        assert_eq!(tokens[0].delta_line, expected.line);
        assert_eq!(tokens[0].delta_start, expected.character);
    }

    #[test]
    fn tokens_are_sorted_by_position_even_across_nested_calls() {
        // An outer call's own bare-word argument, and a nested call's, must
        // come out in source order: `volta` (the outer `\repeat`'s kind) then
        // `unfold` (the nested one's).
        let src = "\\repeat volta 2 { \\repeat unfold 3 { c } }";
        let doc = Document::new(src.to_string());
        let tokens = semantic_tokens_full(&doc);
        assert_eq!(tokens.len(), 2);
        // Both tokens are on the same line, so delta_start accumulates into
        // an absolute character offset directly.
        assert_eq!(tokens[0].delta_line, 0);
        assert_eq!(tokens[1].delta_line, 0);
        assert!(tokens[1].delta_start > 0, "the two tokens must not collide");
    }
}
