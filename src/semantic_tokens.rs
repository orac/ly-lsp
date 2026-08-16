//! `textDocument/semanticTokens/full` support.
//!
//! The TextMate grammar highlights structurally, from token shape alone.
//! In `\repeat volta 2` the `volta` is a `symbol` node like any bare word, but it should be treated differently.
//! Semantic tokens fill that gap: this module walks a document's already-parsed [`Commands`] and emits one token per
//! argument whose meaning the grammar alone can't recover.
//!
//! # What gets a token
//!
//! [`Arg::BareWord`] and [`Arg::Word`] →[`SemanticTokenType::KEYWORD`], as per the `\repeat volta` example above.
//! [`Arg::ContextType`] → [`SemanticTokenType::TYPE`], e.g. `\new Staff`.
//! [`Arg::ContextName`] → [`SemanticTokenType::VARIABLE`], e.g. `\new Staff = horns`.
//! 
//! # What doesn't get a token
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
//!   context in the first place.
//! - [`Arg::Pitch`]: a reference pitch (`\relative c'`, the tonic of `\key c
//!   \major`) is written with exactly the same syntax as a note in the music
//!   body, and is already highlighted as one.
//!
//! # Encoding
//!
//! LSP semantic tokens are delta-encoded relative to the *previous* token
//! (line and start character are both deltas, never absolute) and must be
//! emitted in position order; see [`encode`]. Positions and lengths are UTF-16
//! code units, via [`LineIndex`], not bytes. Emitting more than one token
//! *type* means the spans of each kind can no longer be sorted separately and
//! concatenated — a `\new Staff` inside a `\repeat volta 2` body, or the other
//! way around, interleaves a [`TYPE`](SemanticTokenType::TYPE) token between
//! two [`KEYWORD`](SemanticTokenType::KEYWORD) ones — so [`semantic_tokens_full`]
//! tags every span with its token type up front and sorts the merged sequence
//! once, and [`encode`] carries the type through per token instead of taking
//! one type for the whole call.
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
const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,
    SemanticTokenType::TYPE,
    SemanticTokenType::VARIABLE,
];

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
    let context_type = token_type_index(&SemanticTokenType::TYPE);
    let context_name = token_type_index(&SemanticTokenType::VARIABLE);

    let mut tagged: Vec<(Span, u32)> = doc
        .commands()
        .iter()
        .flat_map(|call| &call.args)
        .filter_map(|arg| match arg {
            Arg::BareWord { span, .. } | Arg::Word { span, .. } => Some((*span, keyword)),
            Arg::ContextType { span, .. } => Some((*span, context_type)),
            Arg::ContextName { span, .. } => Some((*span, context_name)),
            _ => None,
        })
        .collect();
    // This sort keeps a merge of
    // several *kinds* — a `\new Staff` inside a `\repeat volta 2`, or the
    // reverse — in the position order `encode` requires: nothing upstream
    // interleaves the two kinds' spans for us.
    tagged.sort_by_key(|(span, _)| span.start);
    encode(doc.line_index(), &tagged)
}

/// Delta-encodes `tokens` — each a span paired with the semantic token type
/// to emit for it, already sorted by span start — with no modifiers. Each
/// [`SemanticToken`] carries its position as a *delta* from the previous
/// token: `delta_line` relative to the previous token's line, and
/// `delta_start` relative to the previous token's start character on the
/// same line, or from the start of the line otherwise. `lines` converts each
/// span's byte offsets to UTF-16 positions, since that's what LSP counts in,
/// not bytes.
fn encode(lines: &LineIndex, tokens: &[(Span, u32)]) -> Vec<SemanticToken> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for &(span, token_type) in tokens {
        let start = lines.position_at(span.start);
        let end = lines.position_at(span.end);
        debug_assert_eq!(
            start.line, end.line,
            "a bare-word/word/context argument never spans multiple lines"
        );
        let delta_line = start.line - prev_line;
        let delta_start = if delta_line == 0 {
            start.character - prev_start
        } else {
            start.character
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: end.character - start.character,
            token_type,
            token_modifiers_bitset: 0,
        });
        prev_line = start.line;
        prev_start = start.character;
    }
    out
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
    fn tokens_are_sorted_by_position_even_across_nested_calls() {
        // An outer call's own bare-word argument, and a nested call's, must
        // come out in source order: `volta` (the outer `\repeat`'s kind) then
        // `unfold` (the nested one's). Kept as a readable regression anchor
        // for nesting specifically; the property test below covers the
        // general delta/decode contract these hand-written cases each sample
        // a corner of.
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

    #[test]
    fn emits_a_type_token_for_a_context_type() {
        let doc = Document::new("{ \\new Staff { c } }".to_string());
        let tokens = semantic_tokens_full(&doc);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].length, "Staff".encode_utf16().count() as u32);
        assert_eq!(
            tokens[0].token_type,
            token_type_index(&SemanticTokenType::TYPE)
        );
    }

    #[test]
    fn emits_a_variable_token_for_a_context_name() {
        let doc = Document::new("{ \\lyricsto \"vocals\" { la } }".to_string());
        let tokens = semantic_tokens_full(&doc);
        assert_eq!(tokens.len(), 1);
        // Unlike `ContextType::name_span` and `ContextInstance::span` (read
        // by `context.rs`, for a different purpose — see their docs),
        // `Arg::ContextName`'s own span is the whole `string` node, quotes
        // included: `consume_context_name` in `command/mod.rs` reads it that
        // way, and this module tags exactly the span the `Arg` carries
        // rather than trimming it to match.
        assert_eq!(tokens[0].length, "\"vocals\"".encode_utf16().count() as u32);
        assert_eq!(
            tokens[0].token_type,
            token_type_index(&SemanticTokenType::VARIABLE)
        );
    }

    #[test]
    fn interleaves_a_keyword_and_a_type_token_in_position_order_either_way_round() {
        // The case the module docs call out: merging more than one token
        // *kind* means the sort can no longer be done kind by kind — a
        // `\new Staff` nested inside a `\repeat volta 2`'s body puts a TYPE
        // token between the outer call's own two KEYWORD tokens (`volta`
        // here, and none from `\new` itself, so just the one), and the
        // reverse nesting puts a KEYWORD token after a TYPE one. Both
        // directions are checked, since a merge bug could easily get one
        // right and not the other.
        for (src, kinds) in [
            (
                "\\repeat volta 2 { \\new Staff { c } }",
                [SemanticTokenType::KEYWORD, SemanticTokenType::TYPE],
            ),
            (
                "\\new Staff { \\repeat volta 2 { c } }",
                [SemanticTokenType::TYPE, SemanticTokenType::KEYWORD],
            ),
        ] {
            let doc = Document::new(src.to_string());
            let tokens = semantic_tokens_full(&doc);
            assert_eq!(tokens.len(), 2, "source: {src}");
            let types: Vec<u32> = tokens.iter().map(|t| t.token_type).collect();
            let expected: Vec<u32> = kinds.iter().map(token_type_index).collect();
            assert_eq!(types, expected, "source: {src}");
            // Position order: the second token's absolute position must
            // follow the first's, which — both on one line here — delta_start
            // being positive already proves.
            assert_eq!(tokens[0].delta_line, 0);
            assert_eq!(tokens[1].delta_line, 0);
            assert!(tokens[1].delta_start > 0, "tokens out of order for {src}");
        }
    }

    mod proptests {
        use proptest::prelude::*;

        use super::*;

        /// Snippets a generated document is assembled from by joining a random
        /// selection in a random order: command calls with bare-word/word
        /// arguments and `\new`/`\lyricsto` calls with context type/name
        /// arguments (the four kinds `semantic_tokens_full` emits), plain
        /// music with no calls at all, and a comment holding multi-byte
        /// characters (an accented letter and a four-byte musical symbol) so
        /// the UTF-16 conversion is exercised, not just the ASCII case.
        const SNIPPETS: &[&str] = &[
            "\\repeat volta 2 { c }",
            "\\repeat unfold 3 { d }",
            "\\key g \\major",
            "\\new Staff { c }",
            "\\lyricsto \"vocals\" { la }",
            "c d e",
            "% café 𝄞",
        ];

        const JOINERS: &[&str] = &[" ", "\n", "  \n", "\n\n"];

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// LSP semantic tokens are delta-encoded relative to the previous
            /// token and must be emitted in position order, per the module
            /// docs. Decoding the deltas back to absolute positions must
            /// therefore yield a strictly increasing sequence, and every
            /// decoded position and length must agree with what the
            /// document's own `LineIndex` says about the source text at that
            /// token's span — the real contract the three hand-written tests
            /// above each sample one corner of (relative deltas, nesting
            /// order, a multi-byte shift).
            #[test]
            fn deltas_decode_to_positions_matching_the_source(
                snippet_indices in prop::collection::vec(0..SNIPPETS.len(), 1..8),
                joiner_indices in prop::collection::vec(0..JOINERS.len(), 0..8),
            ) {
                let mut src = String::new();
                for (i, &snippet) in snippet_indices.iter().enumerate() {
                    if i > 0 {
                        let joiner = joiner_indices.get(i - 1).copied().unwrap_or(0);
                        src.push_str(JOINERS[joiner]);
                    }
                    src.push_str(SNIPPETS[snippet]);
                }

                let doc = Document::new(src);
                let tokens = semantic_tokens_full(&doc);
                let line_index = doc.line_index();

                // Decode the delta stream back to absolute (line, character) positions.
                let mut decoded = Vec::with_capacity(tokens.len());
                let mut line = 0u32;
                let mut character = 0u32;
                for token in &tokens {
                    character = if token.delta_line == 0 {
                        character + token.delta_start
                    } else {
                        token.delta_start
                    };
                    line += token.delta_line;
                    decoded.push((line, character, token.length));
                }

                // Strictly increasing in (line, character) order.
                for pair in decoded.windows(2) {
                    let (line0, character0, _) = pair[0];
                    let (line1, character1, _) = pair[1];
                    prop_assert!(
                        (line1, character1) > (line0, character0),
                        "tokens out of order: {:?} then {:?}",
                        pair[0],
                        pair[1],
                    );
                }

                // Each decoded position/length must equal what `line_index` derives
                // directly from the corresponding source span, rather than a
                // hard-coded expectation.
                let mut expected_spans: Vec<Span> = doc
                    .commands()
                    .iter()
                    .flat_map(|call| &call.args)
                    .filter_map(|arg| match arg {
                        Arg::BareWord { span, .. }
                        | Arg::Word { span, .. }
                        | Arg::ContextType { span, .. }
                        | Arg::ContextName { span, .. } => Some(*span),
                        _ => None,
                    })
                    .collect();
                expected_spans.sort_by_key(|span| span.start);

                prop_assert_eq!(decoded.len(), expected_spans.len());
                for (&(line, character, length), span) in decoded.iter().zip(&expected_spans) {
                    let start = line_index.position_at(span.start);
                    let end = line_index.position_at(span.end);
                    prop_assert_eq!(line, start.line);
                    prop_assert_eq!(character, start.character);
                    prop_assert_eq!(length, end.character - start.character);
                }
            }
        }
    }
}
