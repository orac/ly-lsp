//! Cursor-driven editor features built on the command table: signature help,
//! argument completion and hover.
//!
//! All three answer the same underlying question — [`Commands::call_site_at`],
//! "which [`CommandCall`] is the cursor in, and at which argument position" —
//! so this module's job is purely to render that one answer into the three
//! different shapes `textDocument/signatureHelp`, `textDocument/completion`
//! and `textDocument/hover` each want. It sits alongside [`document`] rather
//! than growing it, because rendering three LSP response shapes is a
//! self-contained job with no call for [`Document`]'s own parsing/symbol
//! concerns, and a fair amount of formatting logic of its own.
//!
//! [`document`]: crate::document
//! [`Commands::call_site_at`]: crate::command::Commands::call_site_at
//! [`CommandCall`]: crate::command::CommandCall

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, Hover, HoverContents, MarkupContent, MarkupKind,
    ParameterInformation, ParameterLabel, Position, SignatureHelp, SignatureInformation,
};

use crate::command::{ArgKind, CallSite, Candidate, Param};
use crate::document::Document;

/// Resolves `position` to the [`CallSite`] the cursor is in — the "where is
/// the cursor, and what command is it in" question shared by
/// [`signature_help`], [`completions`] and [`hover`], before each renders the
/// answer into its own LSP shape. `None` when there is no command call at
/// `position`, which is `None`/empty for all three.
///
/// Returns the byte offset alongside the [`CallSite`] because [`hover`] needs
/// it for one more check — whether the cursor sits on the keyword itself —
/// that [`Commands::call_site_at`](crate::command::Commands::call_site_at)
/// doesn't answer on its own.
fn call_at(doc: &Document, position: Position) -> Option<(usize, CallSite<'_>)> {
    let offset = doc.line_index().offset_at(position)?;
    let site = doc.commands().call_site_at(offset, doc.text())?;
    Some((offset, site))
}

/// Signature help for the command call at `position`, if the cursor is in
/// one: the command's [`Param`]s rendered as [`ParameterInformation`], with
/// [`SignatureHelp::active_parameter`] set from the [`CallSite`]'s
/// argument index. `None` for a command with no parameters at all (nothing
/// useful to prompt with) as well as when the cursor isn't in a call.
pub fn signature_help(doc: &Document, position: Position) -> Option<SignatureHelp> {
    let (_offset, site) = call_at(doc, position)?;
    let params = site.call.cmd.signature();
    if params.is_empty() {
        return None;
    }

    let active_parameter = (site.index < params.len()).then_some(site.index as u32);
    let signature = SignatureInformation {
        label: signature_label(&site.call.name, params),
        documentation: None,
        parameters: Some(params.iter().map(parameter_information).collect()),
        active_parameter,
    };

    Some(SignatureHelp {
        signatures: vec![signature],
        active_signature: Some(0),
        active_parameter,
    })
}

/// The completion candidates for the argument position at `position`, if the
/// cursor is in one and that parameter has a closed set of accepted values
/// (see [`Command::completions`](crate::command::Command::completions)). Empty
/// otherwise — an open-ended parameter (a pitch, a music block, most strings)
/// offers nothing here rather than guessing.
pub fn completions(doc: &Document, position: Position) -> Vec<CompletionItem> {
    let Some((_offset, site)) = call_at(doc, position) else {
        return Vec::new();
    };
    let cmd = &site.call.cmd;
    let Some(param) = cmd.signature().get(site.index) else {
        return Vec::new();
    };

    cmd.completions(site.index)
        .iter()
        .map(|candidate| completion_item(param, candidate))
        .collect()
}

/// Hover documentation for the command word at `position`, if the cursor sits
/// on one: its signature, plus its curated prose where there is any. `None`
/// when the cursor is elsewhere in a call's header or body — hovering an
/// argument value isn't wired up here, only the command word itself — and
/// `None` for a command with neither parameters nor prose, which is every
/// reference to a plain variable: a popup reading just `\foo` over the `\foo`
/// you are already looking at is worse than nothing. Rendering a variable's
/// *value* would earn one, and is the obvious next thing here.
pub fn hover(doc: &Document, position: Position) -> Option<Hover> {
    let (offset, site) = call_at(doc, position)?;
    if !site.call.keyword.contains(offset) {
        return None;
    }
    let cmd = &site.call.cmd;
    if cmd.signature().is_empty() && cmd.documentation().is_none() {
        return None;
    }

    let mut markdown = format!(
        "```\n{}\n```",
        signature_label(&site.call.name, cmd.signature())
    );
    if let Some(documentation) = cmd.documentation() {
        markdown.push_str("\n\n");
        markdown.push_str(&documentation.markdown);
    }

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(doc.line_index().range_of(site.call.keyword)),
    })
}

/// Renders a command's signature as `\name param [optional]`, the label
/// shared by signature help and hover — an optional [`Param`] shown in
/// brackets, as LilyPond's own manual does.
fn signature_label(name: &str, params: &[Param]) -> String {
    let mut label = format!("\\{name}");
    for param in params {
        label.push(' ');
        if param.optional {
            label.push('[');
            label.push_str(&param.name);
            label.push(']');
        } else {
            label.push_str(&param.name);
        }
    }
    label
}

fn parameter_information(param: &Param) -> ParameterInformation {
    ParameterInformation {
        label: ParameterLabel::Simple(param.name.to_string()),
        documentation: None,
    }
}

/// Renders one [`Candidate`] for `param`, prefixing a leading backslash on
/// insertion for an [`ArgKind::Word`] value (`\major`) — the one place a
/// candidate's on-page label and what actually needs typing differ, since
/// [`Candidate::label`] deliberately carries neither.
fn completion_item(param: &Param, candidate: &Candidate) -> CompletionItem {
    let text = match param.kind {
        ArgKind::Word => format!("\\{}", candidate.label),
        _ => candidate.label.to_string(),
    };
    CompletionItem {
        label: text.clone(),
        kind: Some(CompletionItemKind::VALUE),
        detail: Some(candidate.documentation.to_string()),
        insert_text: Some(text),
        ..CompletionItem::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Position;

    /// Builds a document from `src` (`|` marks the cursor, stripped before
    /// parsing) and returns it with the cursor's position.
    fn doc_at(src: &str) -> (Document, Position) {
        let offset = src.find('|').expect("src must contain a `|` cursor mark");
        let text = format!("{}{}", &src[..offset], &src[offset + 1..]);
        let doc = Document::new(text.clone());
        let position = doc.line_index().position_at(offset);
        (doc, position)
    }

    #[test]
    fn signature_help_shows_the_active_parameter() {
        let (doc, pos) = doc_at("\\repeat volta |");
        let help = signature_help(&doc, pos).expect("signature help");
        assert_eq!(help.signatures.len(), 1);
        assert_eq!(help.signatures[0].label, "\\repeat kind count music");
        assert_eq!(help.active_parameter, Some(1));
    }

    #[test]
    fn signature_help_marks_optional_parameters() {
        let (doc, pos) = doc_at("\\relative |");
        let help = signature_help(&doc, pos).expect("signature help");
        assert_eq!(help.signatures[0].label, "\\relative [reference] music");
    }

    #[test]
    fn signature_help_none_outside_a_call() {
        let (doc, pos) = doc_at("c d |e");
        assert!(signature_help(&doc, pos).is_none());
    }

    #[test]
    fn completions_offers_repeat_kinds() {
        let (doc, pos) = doc_at("\\repeat |");
        let items = completions(&doc, pos);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["volta", "unfold", "percent", "tremolo", "segno"]
        );
    }

    #[test]
    fn completions_prefixes_a_backslash_for_word_arguments() {
        let (doc, pos) = doc_at("\\key c |");
        let items = completions(&doc, pos);
        assert!(items.iter().any(|i| i.label == "\\major"));
        assert!(items.iter().any(|i| i.label == "\\minor"));
    }

    #[test]
    fn completions_empty_for_an_open_ended_parameter() {
        // `\repeat`'s `count` (index 1) has no closed set of values.
        let (doc, pos) = doc_at("\\repeat volta 2|");
        assert!(completions(&doc, pos).is_empty());
    }

    #[test]
    fn completions_empty_outside_a_call() {
        let (doc, pos) = doc_at("c d |e");
        assert!(completions(&doc, pos).is_empty());
    }

    #[test]
    fn hover_on_the_keyword_shows_signature_and_documentation() {
        let (doc, pos) = doc_at("\\rela|tive c' { c }");
        let hover = hover(&doc, pos).expect("hover");
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(markup.value.contains("\\relative [reference] music"));
        assert!(markup.value.contains("relative to the previous note"));
    }

    #[test]
    fn hover_stays_quiet_over_a_plain_variable() {
        // `\foo` resolves to a zero-argument command with nothing to say about
        // itself. A popup rendering just `\foo` over the `\foo` under the
        // cursor is worse than no popup at all.
        let (doc, pos) = doc_at("foo = { c }\n\\f|oo\n");
        assert!(hover(&doc, pos).is_none());
    }

    #[test]
    fn hover_none_on_an_argument() {
        let (doc, pos) = doc_at("\\repeat vol|ta 2 { c }");
        assert!(hover(&doc, pos).is_none());
    }

    #[test]
    fn hover_none_outside_a_call() {
        let (doc, pos) = doc_at("c d |e");
        assert!(hover(&doc, pos).is_none());
    }
}
