//! Respells `\%`/`\*` — LilyPond 2.26's abbreviations for `\repeat percent`/
//! `\repeat unfold` — as the longhand form, for a reader unfamiliar with the
//! shorthand. Just the keyword is rewritten: `count`/`music` already follow it
//! in the shape `\repeat kind` itself expects, so nothing else in the call
//! changes.

use tower_lsp::lsp_types::{CodeActionKind, Range, TextEdit};

use crate::command::CommandCall;
use crate::document::Document;

use super::{CodeAction, Offer, Resolved};

pub struct ExpandRepeatAbbreviation;

impl CodeAction for ExpandRepeatAbbreviation {
    const ID: &'static str = "expand_repeat_abbreviation";

    fn offer(document: &Document, selection: Range) -> Option<Offer> {
        let (_, kind) = abbreviation_at(document, selection)?;
        Some(Offer {
            title: format!("Replace with `\\repeat {kind}`"),
            kind: CodeActionKind::REFACTOR_REWRITE,
        })
    }

    fn resolve(document: &Document, selection: Range) -> Option<Resolved> {
        let (call, kind) = abbreviation_at(document, selection)?;
        Some(Resolved {
            edits: vec![TextEdit {
                range: document.line_index().range_of(call.keyword),
                new_text: format!("\\repeat {kind}"),
            }],
            rename_at: None,
        })
    }
}

/// The `\%`/`\*` call the cursor sits on, and the `\repeat` kind it stands
/// for.
fn abbreviation_at(document: &Document, selection: Range) -> Option<(&CommandCall, &'static str)> {
    let offset = document.line_index().offset_at(selection.start)?;
    let call = document.commands().iter().find(|call| {
        matches!(call.name.as_str(), "%" | "*")
            && call.keyword.start <= offset
            && offset <= call.keyword.end
    })?;
    let kind = match call.name.as_str() {
        "%" => "percent",
        "*" => "unfold",
        _ => unreachable!("filtered to these two names above"),
    };
    Some((call, kind))
}

#[cfg(test)]
mod tests {
    use tower_lsp::lsp_types::Position;

    use super::*;
    use crate::document::Document;

    fn document(src: &str) -> Document {
        Document::new(src.to_string())
    }

    fn at(line: u32, character: u32) -> Range {
        let position = Position::new(line, character);
        Range::new(position, position)
    }

    #[test]
    fn offers_nothing_away_from_an_abbreviation() {
        let doc = document("\\repeat percent 2 { c4 }");
        assert!(ExpandRepeatAbbreviation::offer(&doc, at(0, 0)).is_none());
    }

    #[test]
    fn expands_percent() {
        let doc = document("\\% 2 { c4 }");
        assert!(ExpandRepeatAbbreviation::offer(&doc, at(0, 1)).is_some());
        let resolved = ExpandRepeatAbbreviation::resolve(&doc, at(0, 1)).expect("resolves");
        assert_eq!(resolved.edits.len(), 1);
        assert_eq!(resolved.edits[0].new_text, "\\repeat percent");
    }

    #[test]
    fn expands_unfold() {
        let doc = document("\\* 2 { c4 }");
        let resolved = ExpandRepeatAbbreviation::resolve(&doc, at(0, 1)).expect("resolves");
        assert_eq!(resolved.edits[0].new_text, "\\repeat unfold");
    }
}
