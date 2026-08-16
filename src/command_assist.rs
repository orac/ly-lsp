//! Cursor-driven editor features built on the command table: signature help,
//! completion and hover.
//!
//! Mostly they answer the same underlying question — [`Commands::call_site_at`],
//! "which [`CommandCall`] is the cursor in, and at which argument position" —
//! so this module's job is largely to render that one answer into the three
//! different shapes `textDocument/signatureHelp`, `textDocument/completion`
//! and `textDocument/hover` each want. Completing a *name* is the exception,
//! and has to be: a half-typed `\rela` is no call at all, so that answer comes
//! from the document's [`Scope`] instead. It sits alongside [`document`] rather
//! than growing it, because rendering three LSP response shapes is a
//! self-contained job with no call for [`Document`]'s own parsing/symbol
//! concerns, and a fair amount of formatting logic of its own.
//!
//! [`document`]: crate::document
//! [`Commands::call_site_at`]: crate::command::Commands::call_site_at
//! [`CommandCall`]: crate::command::CommandCall

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, Hover, HoverContents,
    MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, Position, Range,
    SignatureHelp, SignatureInformation, TextEdit,
};

use crate::command::{
    ArgKind, CallSite, Candidate, Command, CompletionContext, Param, signature_label,
};
use crate::document::Document;
use crate::line_struct::Span;
use crate::vocabulary::Scope;

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

/// What can be written at `position`, which is one of three quite different
/// questions depending on where the cursor is.
///
/// The narrower answer comes first: at an argument position whose parameter
/// has a closed set of accepted values, those values and nothing else. So
/// `\key c \|` still offers the nine modes rather than burying them in every
/// command in the language.
///
/// Failing that — including at a bare `\new`/`\context`/`\lyricsto`/`\change`
/// with nothing typed after it, which parses too poorly for the first route
/// to find at all — [`context_argument_completions`] tries the same
/// parameter-0 candidates by reading the keyword straight out of the text.
///
/// Failing that too, a cursor inside a `\word` is naming a command, and the
/// answer is every command the document's [`Scope`] can resolve — the user's
/// own definitions, their LilyPond's, and the bare names from its word list —
/// each labelled with where it came from.
///
/// Everywhere else, nothing: an open-ended parameter (a pitch, a music block,
/// most strings) is better left alone than guessed at.
pub fn completions(
    doc: &Document,
    position: Position,
    ctx: &CompletionContext,
) -> Vec<CompletionItem> {
    let arguments = argument_completions(doc, position, ctx);
    if !arguments.is_empty() {
        return arguments;
    }

    let Some(offset) = doc.line_index().offset_at(position) else {
        return Vec::new();
    };

    let unparsed = context_argument_completions(doc, offset, ctx);
    if !unparsed.is_empty() {
        return unparsed;
    }

    match word_being_typed(doc.text(), offset) {
        Some(typed) => command_names(doc.scope(), doc.line_index().range_of(typed)),
        None => Vec::new(),
    }
}

/// The fallback [`argument_completions`] can't reach: a bare `\new` or
/// `\context` with nothing typed after it yet.
///
/// `dump_tree` on `\new ` shows why — tree-sitter's error recovery leaves it
/// wrapped in an `ERROR` node rather than the `named_context` shape
/// [`Commands::call_site_at`](crate::command::Commands::call_site_at) needs,
/// and `note_analyser::Analyser::walk` never descends into an `ERROR` node at
/// all (it only recurses into `expression_block`/`parallel_music`), so no
/// [`CommandCall`](crate::command::CommandCall) is ever built for this
/// position — `call_at` finds nothing, exactly the situation
/// [`word_being_typed`]'s own doc describes for a half-typed `\word`, except
/// here there is no partial word for that scan to find either, since a
/// context type carries no backslash. `\context` on its own doesn't hit this
/// hole — LilyPond's grammar recovers it as a plain `escaped_word`, which
/// `argument_completions` already handles — nor do `\lyricsto`/`\change`,
/// checked the same way with `dump_tree`. All four are still read for here,
/// small as the extra cost is, as a guard against a future grammar change
/// moving the hole: whichever of them precedes the cursor, with nothing but
/// whitespace in between, resolves the same [`Command`] the tree path would
/// have found, and asks it for parameter 0's candidates exactly as
/// [`argument_completions`] does — so a context type's or instance's
/// candidates and documentation come from one place, not two.
fn context_argument_completions(
    doc: &Document,
    offset: usize,
    ctx: &CompletionContext,
) -> Vec<CompletionItem> {
    let Some(keyword) = keyword_awaiting_its_first_argument(doc.text(), offset) else {
        return Vec::new();
    };
    let Some(known) = doc.scope().get(keyword) else {
        return Vec::new();
    };
    let Some(param) = known.value.signature().first() else {
        return Vec::new();
    };
    known
        .value
        .completions(0, ctx)
        .iter()
        .map(|candidate| completion_item(param, candidate))
        .collect()
}

/// The name (without its backslash) of `\new`, `\context`, `\lyricsto` or
/// `\change` immediately before `offset`, with nothing but whitespace between
/// the keyword and the cursor — see [`context_argument_completions`] for why
/// these four are checked here at all.
fn keyword_awaiting_its_first_argument(src: &str, offset: usize) -> Option<&'static str> {
    const KEYWORDS: &[&str] = &["\\new", "\\context", "\\lyricsto", "\\change"];
    let before = src
        .get(..offset)?
        .trim_end_matches(|c: char| c.is_ascii_whitespace());
    KEYWORDS.iter().find_map(|&keyword| {
        let rest = before.strip_suffix(keyword)?;
        // A real word boundary before the backslash — `\newer` mustn't match
        // `\new` — the same care `word_being_typed` takes reading the other
        // direction.
        let boundary = rest
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '\\'));
        boundary.then_some(&keyword[1..])
    })
}

/// The values the command at `position` accepts at the argument the cursor is
/// in — see [`Command::completions`](crate::command::Command::completions).
///
/// Nothing while the cursor is still in the command word itself: `\ver|sion`
/// reports argument 0 (there is nowhere else for a cursor in a call to be),
/// but the thing being typed there is the name, not what follows it.
fn argument_completions(
    doc: &Document,
    position: Position,
    ctx: &CompletionContext,
) -> Vec<CompletionItem> {
    let Some((offset, site)) = call_at(doc, position) else {
        return Vec::new();
    };
    if site.call.keyword.contains(offset) {
        return Vec::new();
    }
    let cmd = &site.call.cmd;
    let Some(param) = cmd.signature().get(site.index) else {
        return Vec::new();
    };

    cmd.completions(site.index, ctx)
        .iter()
        .map(|candidate| completion_item(param, candidate))
        .collect()
}

/// The span of the `\word` the cursor is in the middle of writing: from a
/// backslash to `offset`, with only name characters between. `None` when the
/// cursor is anywhere else.
///
/// Read straight from the text rather than from the parse tree, because the
/// point of asking is to complete a name that isn't finished and so very
/// likely isn't a command yet: a half-typed `\rela` resolves to nothing, and
/// tree-sitter has no call there for [`call_at`] to find.
fn word_being_typed(src: &str, offset: usize) -> Option<Span> {
    let before = src.get(..offset)?;
    let start = before
        .rfind(|c: char| !(c.is_alphanumeric() || c == '-'))
        .filter(|&at| before.as_bytes()[at] == b'\\')?;
    Some(Span::new(start, offset))
}

/// Every command the document can see, as completion items replacing `range`
/// — the `\word` written so far, backslash and all, so that accepting one
/// doesn't leave the backslash doubled.
///
/// Each says where it came from in its `detail`, which is the line the client
/// shows beside the label: the same attribution [`hover`] leads with, and the
/// answer to "whose `\foo` is this?" while choosing between two of them.
fn command_names(scope: &Scope, range: Range) -> Vec<CompletionItem> {
    scope
        .visible()
        .into_iter()
        .map(|(name, known)| {
            let text = format!("\\{name}");
            CompletionItem {
                label: text.clone(),
                // A LilyPond command is a binding like any other: one that
                // takes arguments is a music function, one that doesn't is a
                // variable holding music.
                kind: Some(if known.value.signature().is_empty() {
                    CompletionItemKind::VARIABLE
                } else {
                    CompletionItemKind::FUNCTION
                }),
                detail: Some(known.layer.origin().to_string()),
                documentation: describe(known.value.as_ref()).map(|value| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value,
                    })
                }),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: text,
                })),
                ..CompletionItem::default()
            }
        })
        .collect()
}

/// Hover documentation for the command word at `position`, if the cursor sits
/// on one: where the command came from, what it is — a signature for most
/// commands, a summary of the value for a variable — and its documentation
/// where there is any. `None` when the cursor is elsewhere in a call's header or
/// body — hovering an argument value isn't wired up here, only the command word
/// itself — and `None` for a command with neither
/// [`synopsis`](Command::synopsis) nor documentation: a popup reading just
/// `\foo` over the `\foo` you are already looking at is worse than nothing.
pub fn hover(doc: &Document, position: Position) -> Option<Hover> {
    let (offset, site) = call_at(doc, position)?;
    if !site.call.keyword.contains(offset) {
        return None;
    }
    let cmd = &site.call.cmd;
    let described = describe(cmd.as_ref())?;

    // Italic and above the synopsis: where a command comes from is a question
    // about it rather than part of what it says, so it reads as an attribution
    // rather than as code.
    let markdown = format!("*{}*\n\n{described}", site.call.origin);

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(doc.line_index().range_of(site.call.keyword)),
    })
}

/// What a command *is*, as Markdown: its [`synopsis`](Command::synopsis), and
/// its documentation where there is any, ruled off from each other so the prose
/// doesn't read as a continuation of the code. Shared by [`hover`], which puts
/// the command's origin above it, and by the name completions, which show it in
/// the detail pane beside the list. `None` for a command that has neither, which
/// nothing should be showing a popup for.
fn describe(cmd: &dyn Command) -> Option<String> {
    let synopsis = cmd.synopsis();
    let documentation = cmd.documentation().map(|doc| doc.markdown.clone());
    match (synopsis, documentation) {
        (Some(synopsis), Some(documentation)) => {
            Some(format!("{synopsis}\n\n---\n\n{documentation}"))
        }
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

fn parameter_information(param: &Param) -> ParameterInformation {
    ParameterInformation {
        label: ParameterLabel::Simple(param.name.to_string()),
        documentation: None,
    }
}

/// Renders one [`Candidate`] for `param`, adding whatever punctuation the
/// parameter it fills calls for: a leading backslash for an [`ArgKind::Word`]
/// value (`\major`), quotes for an [`ArgKind::String`] (`"2.24.3"`). That's
/// the one place a candidate's on-page label and what actually needs typing
/// differ, since [`Candidate::label`] deliberately carries neither.
fn completion_item(param: &Param, candidate: &Candidate) -> CompletionItem {
    let text = match param.kind {
        ArgKind::Word => format!("\\{}", candidate.label),
        // A context instance name is quoted on insertion even though a bare
        // symbol parses identically (`\new Voice = vocals` is as valid as
        // `\new Voice = "vocals"`) — see the rationale on `ArgKind::ContextName`
        // itself for why quoted is the one to write.
        ArgKind::String | ArgKind::ContextName => format!("\"{}\"", candidate.label),
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

    /// Splits `src` at its `|` cursor mark into the text without it and the
    /// offset it stood at.
    fn cursor(src: &str) -> (String, usize) {
        let offset = src.find('|').expect("src must contain a `|` cursor mark");
        (format!("{}{}", &src[..offset], &src[offset + 1..]), offset)
    }

    /// Builds a document from `src` (`|` marks the cursor, stripped before
    /// parsing) and returns it with the cursor's position.
    fn doc_at(src: &str) -> (Document, Position) {
        let (text, offset) = cursor(src);
        let doc = Document::new(text);
        let position = doc.line_index().position_at(offset);
        (doc, position)
    }

    /// The markdown of the hover at `position`, which there must be one of.
    fn markup_of(doc: &Document, position: Position) -> String {
        let HoverContents::Markup(markup) = hover(doc, position).expect("hover").contents else {
            panic!("expected markup content");
        };
        markup.value
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

    /// A workspace with no installation behind it, which is what every test
    /// here but [`the_version_argument_completes_to_the_installed_version`]
    /// wants: it makes no difference to any completion but `\version`'s.
    /// Takes `doc` because [`CompletionContext::scope`] must be the scope the
    /// completion is actually being asked about — [`Document::scope`] is
    /// `pub(crate)`, so this reaches it exactly as `document_graph.rs` does.
    fn no_install(doc: &Document) -> CompletionContext<'_> {
        CompletionContext {
            lilypond_version: None,
            scope: doc.scope(),
        }
    }

    /// The labels of the completions at `position`, in the order offered.
    fn labels_at(doc: &Document, position: Position, ctx: &CompletionContext) -> Vec<String> {
        completions(doc, position, ctx)
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    #[test]
    fn completions_offers_repeat_kinds() {
        let (doc, pos) = doc_at("\\repeat |");
        assert_eq!(
            labels_at(&doc, pos, &no_install(&doc)),
            vec!["volta", "unfold", "percent", "tremolo", "segno"]
        );
    }

    #[test]
    fn completions_prefixes_a_backslash_for_word_arguments() {
        let (doc, pos) = doc_at("\\key c |");
        let labels = labels_at(&doc, pos, &no_install(&doc));
        assert!(labels.iter().any(|label| label == "\\major"));
        assert!(labels.iter().any(|label| label == "\\minor"));
    }

    #[test]
    fn a_closed_set_of_argument_values_beats_the_whole_vocabulary() {
        // The cursor is inside a `\word`, so command names are on offer in
        // principle — but this one can only be a mode, and every command in
        // the language would bury the nine that fit.
        let (doc, pos) = doc_at("\\key c \\m|");
        let labels = labels_at(&doc, pos, &no_install(&doc));
        assert!(labels.iter().any(|label| label == "\\major"));
        assert!(!labels.iter().any(|label| label == "\\relative"));
    }

    #[test]
    fn completions_empty_for_an_open_ended_parameter() {
        // `\repeat`'s `count` (index 1) has no closed set of values.
        let (doc, pos) = doc_at("\\repeat volta 2|");
        assert!(completions(&doc, pos, &no_install(&doc)).is_empty());
    }

    #[test]
    fn completions_empty_outside_a_call() {
        let (doc, pos) = doc_at("c d |e");
        assert!(completions(&doc, pos, &no_install(&doc)).is_empty());
    }

    #[test]
    fn a_half_typed_command_completes_to_every_name_in_scope() {
        // `\rela` resolves to nothing, so there is no call here to read a
        // signature from: the whole vocabulary is the answer, and the client
        // narrows it to what has been typed.
        let (doc, pos) = doc_at("{ \\rela| }");
        let labels = labels_at(&doc, pos, &no_install(&doc));
        assert!(labels.iter().any(|label| label == "\\relative"));
        assert!(labels.iter().any(|label| label == "\\repeat"));
    }

    #[test]
    fn a_name_completion_says_where_the_command_came_from() {
        let (text, offset) = cursor("foo = { c }\n{ \\f| }\n");
        let doc = Document::named("song.ly", text);
        let items = completions(
            &doc,
            doc.line_index().position_at(offset),
            &no_install(&doc),
        );
        let own = items.iter().find(|item| item.label == "\\foo").unwrap();
        assert_eq!(own.detail.as_deref(), Some("song.ly"));
        let built_in = items
            .iter()
            .find(|item| item.label == "\\relative")
            .unwrap();
        assert_eq!(built_in.detail.as_deref(), Some("built-in"));
    }

    #[test]
    fn a_name_completion_replaces_the_backslash_already_typed() {
        // On pain of `\\relative`: the range must reach back over the `\`,
        // which the editor's own idea of a word may well not include.
        let (doc, pos) = doc_at("{ \\rela| }");
        let items = completions(&doc, pos, &no_install(&doc));
        let item = items
            .iter()
            .find(|item| item.label == "\\relative")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
            panic!("expected a text edit");
        };
        assert_eq!(edit.range.start.character, 2);
        assert_eq!(edit.range.end.character, 7);
        assert_eq!(edit.new_text, "\\relative");
    }

    #[test]
    fn rewriting_a_command_name_completes_the_name_not_its_argument() {
        // The cursor is in `\version`'s keyword, which is argument position 0
        // as far as the call goes — but what's being typed is the name.
        let (doc, pos) = doc_at("\\ver|sion \"2.24.3\"\n");
        let ctx = CompletionContext {
            lilypond_version: Some("2.24.3"),
            scope: doc.scope(),
        };
        let labels = labels_at(&doc, pos, &ctx);
        assert!(labels.iter().any(|label| label == "\\version"));
        assert!(!labels.iter().any(|label| label == "\"2.24.3\""));
    }

    #[test]
    fn the_version_argument_completes_to_the_installed_version() {
        let (doc, pos) = doc_at("\\version |");
        let ctx = CompletionContext {
            lilypond_version: Some("2.24.3"),
            scope: doc.scope(),
        };
        assert_eq!(labels_at(&doc, pos, &ctx), vec!["\"2.24.3\""]);
    }

    #[test]
    fn the_version_argument_offers_nothing_without_an_installation() {
        let (doc, pos) = doc_at("\\version |");
        assert!(completions(&doc, pos, &no_install(&doc)).is_empty());
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
    fn hover_rules_the_documentation_off_from_the_synopsis() {
        // Prose immediately under a code fence reads as a continuation of it.
        let (doc, pos) = doc_at("\\rela|tive c' { c }");
        assert!(markup_of(&doc, pos).contains("\n---\n"));
    }

    #[test]
    fn hover_over_a_variable_shows_what_it_is_bound_to() {
        // A zero-argument command has no signature worth reading, so the value
        // is the whole point of the popup; what it looks like is
        // [`Variable`](crate::command::variable::Variable)'s business.
        let (doc, pos) = doc_at("foo = { c }\n\\f|oo\n");
        let hover = hover(&doc, pos).expect("hover");
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(markup.value.contains("foo = { c }"), "{}", markup.value);
    }

    #[test]
    fn hover_over_a_variable_shows_the_value_and_nothing_else() {
        // On pain of a popup whose first line is the `\foo` being hovered: a
        // variable's synopsis replaces the signature rather than joining it.
        let (doc, pos) = doc_at("foo = { c }\n\\f|oo\n");
        let markup = markup_of(&doc, pos);
        assert!(!markup.contains("\\foo"), "{markup}");
    }

    #[test]
    fn hover_names_the_file_a_command_was_defined_in() {
        // The first line of every hover says where the knowledge came from;
        // for a definition in a file, that is the file's name.
        let (text, offset) = cursor("foo = { c }\n\\f|oo\n");
        let doc = Document::named("song.ly", text);
        let markup = markup_of(&doc, doc.line_index().position_at(offset));
        assert!(markup.starts_with("*song.ly*"), "{markup}");
    }

    #[test]
    fn hover_over_our_own_knowledge_says_so() {
        let (doc, pos) = doc_at("\\rela|tive c' { c }");
        assert!(markup_of(&doc, pos).starts_with("*built-in*"));
    }

    #[test]
    fn a_document_with_no_file_behind_it_still_says_what_it_is() {
        let (doc, pos) = doc_at("foo = { c }\n\\f|oo\n");
        assert!(markup_of(&doc, pos).starts_with("*untitled*"));
    }

    #[test]
    fn hover_stays_quiet_over_a_name_with_nothing_behind_it() {
        // `\break` is known from the word list alone: no parameters, no prose,
        // no value to show. A popup rendering just `\break` over the `\break`
        // under the cursor is worse than no popup at all.
        let (doc, pos) = doc_at("{ c \\bre|ak }");
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

    /// A document that declares its own context type, the way a
    /// `\layout { \context { … } }` block would — enough to exercise
    /// `Scope::visible_context_types` without needing a real installation
    /// behind it (`TESTING.md`: only the installation-*dependent* cases need
    /// one, and which context types exist and what they're named isn't one of
    /// those here).
    fn doc_with_a_declared_context(src: &str) -> (Document, Position) {
        doc_at(&format!(
            "\\layout {{ \\context {{ \\name MyStaff \\description \"Custom staff.\" }} }}\n{src}"
        ))
    }

    #[test]
    fn new_with_nothing_typed_offers_the_documents_own_context_types() {
        // The dead spot `dump_tree` found: tree-sitter wraps a bare `\new`
        // with nothing after it in an `ERROR` node, so `argument_completions`
        // has no parsed call to work from at all — this is
        // `context_argument_completions`'s one job.
        let (doc, pos) = doc_with_a_declared_context("{ \\new |}");
        let items = completions(&doc, pos, &no_install(&doc));
        let my_staff = items
            .iter()
            .find(|item| item.label == "MyStaff")
            .expect("MyStaff should be offered");
        assert_eq!(my_staff.detail.as_deref(), Some("Custom staff."));
    }

    #[test]
    fn new_with_a_type_prefix_still_offers_matching_context_types() {
        // Once even one character of the type is typed, tree-sitter recovers
        // a real `named_context` node, so this goes through
        // `argument_completions` — the ordinary tree-based path — rather than
        // the text-level fallback the previous test exercises.
        let (doc, pos) = doc_with_a_declared_context("{ \\new MyS| }");
        let items = completions(&doc, pos, &no_install(&doc));
        assert!(items.iter().any(|item| item.label == "MyStaff"));
    }

    #[test]
    fn context_declares_no_type_offers_no_completions_deep_in_a_body() {
        // Between two notes, with no argument position to complete — the
        // same "nowhere to complete" case `completions_empty_outside_a_call`
        // covers for an ordinary call, checked here for `\new`'s body too.
        let (doc, pos) = doc_with_a_declared_context("{ \\new Staff { c |d } }");
        assert!(completions(&doc, pos, &no_install(&doc)).is_empty());
    }

    #[test]
    fn internal_context_types_are_excluded_from_the_offer_but_still_known() {
        // `InternalGregorianStaff`/`InternalMensuralStaff` are real LilyPond
        // context types (see `is_internal_context_type`'s doc for how that
        // was checked against installed shares) that no score writes
        // `\new`/`\context` against directly. A document can declare one
        // itself — the shape of the exclusion doesn't depend on where the
        // type came from — which is what lets this run without an
        // installation.
        let (doc, pos) = doc_at("\\layout { \\context { \\name InternalFoo } }\n{ \\new |}");
        let items = completions(&doc, pos, &no_install(&doc));
        assert!(
            !items.iter().any(|item| item.label == "InternalFoo"),
            "an Internal* type must not be offered: {items:?}"
        );
        assert!(
            doc.scope().get_context_type("InternalFoo").is_some(),
            "an Internal* type must still be known, on pain of a false undefined-reference"
        );
    }

    #[test]
    fn lyricsto_with_nothing_typed_offers_context_instance_names() {
        let (doc, pos) = doc_at("{ \\new Voice = \"vocals\" { c } \\lyricsto |{ la } }");
        let items = completions(&doc, pos, &no_install(&doc));
        let vocals = items
            .iter()
            .find(|item| item.label == "\"vocals\"")
            .expect("vocals should be offered, quoted");
        assert_eq!(
            vocals.detail.as_deref(),
            Some("Voice"),
            "the instance's documentation names the type it was created as"
        );
    }

    #[test]
    fn news_name_position_offers_context_instance_names_too() {
        // `= "name"` is the same context-instance namespace `\lyricsto`
        // reads, reached through the ordinary tree path (the flattened
        // `named_context` shape `command::parse` documents) rather than the
        // text-level fallback.
        let (doc, pos) = doc_at("{ \\new Voice = \"vocals\" { c } } { \\new Staff = \"|\" }");
        let items = completions(&doc, pos, &no_install(&doc));
        assert!(items.iter().any(|item| item.label == "\"vocals\""));
    }

    #[test]
    fn change_offers_context_types_not_instance_names_at_its_first_argument() {
        // `\change type = name`: index 0 is a context type, unlike
        // `\lyricsto`'s and `\new`'s `= "name"`, which name an instance.
        let (doc, pos) = doc_with_a_declared_context("{ \\change |}");
        let items = completions(&doc, pos, &no_install(&doc));
        assert!(items.iter().any(|item| item.label == "MyStaff"));
    }

    #[test]
    fn an_unrelated_bare_reserved_word_is_unaffected_by_the_context_fallback() {
        // `keyword_awaiting_its_first_argument` only recognises
        // `\new`/`\context`/`\lyricsto`/`\change`; a bare `\repeat` must keep
        // going through the ordinary route rather than being swallowed by
        // the new fallback.
        let (doc, pos) = doc_at("\\repeat |");
        let labels = labels_at(&doc, pos, &no_install(&doc));
        assert_eq!(
            labels,
            vec!["volta", "unfold", "percent", "tremolo", "segno"]
        );
    }
}
