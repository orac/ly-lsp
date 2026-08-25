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
    Arg, ArgKind, CallSite, Candidate, Command, CompletionContext, Param, signature_label,
};
use crate::context::{ContextInstance, ContextType};
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

/// Hover documentation for the name at `position`.
///
/// On a command word: where the command came from, what it is — a signature
/// for most commands, a summary of the value for a variable — and its
/// documentation where there is any. `None` for a command with neither
/// [`synopsis`](Command::synopsis) nor documentation: a popup reading just
/// `\foo` over the `\foo` you are already looking at is worse than nothing.
///
/// On a context type or instance argument — the `Staff` of `\new Staff`, the
/// `"vocals"` of `\lyricsto "vocals"` — [`context_hover`] answers instead,
/// and is tried first: those arguments are the one part of a call's header
/// that names something in its own right, so the "only the command word"
/// rule the rest of a call still follows would be answering about `\new`
/// while the cursor is on `Staff`.
///
/// `None` anywhere else in a call's header or body.
pub fn hover(doc: &Document, position: Position) -> Option<Hover> {
    if let Some(hover) = context_hover(doc, position) {
        return Some(hover);
    }

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

/// Hover documentation for the context type or instance at `position`, if the
/// cursor sits on one: the same shape as a command's hover — where the name
/// came from, then what it is — for the two namespaces
/// [`Document::context_arg_at`] recognises, which no [`CallSite`] of their
/// own would answer for. `None` when the cursor is anywhere else, and when
/// the name resolves to nothing the scope knows.
fn context_hover(doc: &Document, position: Position) -> Option<Hover> {
    let arg = doc.context_arg_at(position)?;
    let described = match arg {
        Arg::ContextType { name, .. } => {
            let known = doc.scope().get_context_type(name)?;
            attribute(known.layer.origin(), describe_context_type(known.value)?)
        }
        Arg::ContextName { name, .. } => {
            let known = doc.scope().get_context_instance(name)?;
            attribute(
                known.layer.origin(),
                describe_context_instance(known.value)?,
            )
        }
        _ => return None,
    };

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: described,
        }),
        range: Some(doc.line_index().range_of(arg.span())),
    })
}

/// `described`, with where it came from italicised above it — the attribution
/// every hover this module renders leads with, so a context type and a
/// command say where they came from the same way.
fn attribute(origin: &str, described: String) -> String {
    format!("*{origin}*\n\n{described}")
}

/// What a context type *is*, as Markdown: the name and the aliases it also
/// answers to, then its `\description` where it has one, ruled off from each
/// other exactly as [`describe`] rules a command's synopsis off from its
/// documentation.
///
/// `None` for a type with neither a description nor an alias — a popup
/// reading "`MyStaff` context" over the `MyStaff` you are already looking at
/// is worse than nothing, the same rule [`hover`] follows for a command with
/// nothing to say.
fn describe_context_type(context_type: &ContextType) -> Option<String> {
    let synopsis = match context_type.aliases.as_slice() {
        [] => return context_type.description.clone(),
        aliases => {
            let aliases: Vec<String> = aliases.iter().map(|alias| format!("`{alias}`")).collect();
            format!(
                "`{}` context, also known as {}",
                context_type.name,
                aliases.join(", ")
            )
        }
    };
    match &context_type.description {
        Some(description) => Some(format!("{synopsis}\n\n---\n\n{description}")),
        None => Some(synopsis),
    }
}

/// What a context instance *is*: which type it was created as, which is the
/// one thing about it a `\change` or `\lyricsto` site doesn't say for itself.
/// `None` where [`ContextInstance::type_name`] is — a half-typed `\new =
/// "vocals"` names no type to report.
fn describe_context_instance(instance: &ContextInstance) -> Option<String> {
    let type_name = instance.type_name.as_ref()?;
    Some(format!("`{}` — a `{type_name}` context", instance.name))
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

/// Resolves an [`ArgKind::Group`] to the kind of its first non-[`Literal`](ArgKind::Literal)
/// piece — the one a candidate could actually belong to, `=` never offering
/// any — so [`completion_item`] can decide insertion punctuation for it the
/// same way it would for a plain parameter. `\new`/`\context`/`\change`'s
/// `= name` clause is the only group with completions today, and its one
/// candidate-bearing piece is `name`; anything else passes through
/// unchanged.
fn resolve_group(kind: &ArgKind) -> &ArgKind {
    match kind {
        ArgKind::Group(sub) => sub
            .iter()
            .map(|p| &p.kind)
            .find(|k| !matches!(k, ArgKind::Literal(_)))
            .unwrap_or(kind),
        other => other,
    }
}

/// Renders one [`Candidate`] for `param`, adding whatever punctuation the
/// parameter it fills calls for: a leading backslash for an [`ArgKind::Word`]
/// value (`\major`), quotes for an [`ArgKind::String`] (`"2.24.3"`). That's
/// the one place a candidate's on-page label and what actually needs typing
/// differ, since [`Candidate::label`] deliberately carries neither.
fn completion_item(param: &Param, candidate: &Candidate) -> CompletionItem {
    let text = match resolve_group(&param.kind) {
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

    #[test]
    fn signature_help_shows_grouped_clauses_as_one_bracket() {
        // `duration = value` and `= name` are each an `ArgKind::Group`: one
        // optional clause, not two or three independently optional pieces.
        let (doc, pos) = doc_at("\\tempo |");
        let help = signature_help(&doc, pos).expect("signature help");
        assert_eq!(
            help.signatures[0].label,
            "\\tempo [text] [duration = value]"
        );

        let (doc, pos) = doc_at("{ \\new Staff |}");
        let help = signature_help(&doc, pos).expect("signature help");
        assert_eq!(help.signatures[0].label, "\\new type [= name] [with] music");
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
    fn hover_over_a_context_type_shows_its_description() {
        let (doc, pos) = doc_with_a_declared_context("{ \\new My|Staff { c } }");
        assert_eq!(markup_of(&doc, pos), "*untitled*\n\nCustom staff.");
    }

    #[test]
    fn hover_over_a_context_type_lists_the_aliases_it_also_answers_to() {
        let (doc, pos) = doc_at(
            "\\layout { \\context { \\name MyStaff \\alias Staff \\description \"Custom staff.\" } }\n{ \\new My|Staff { c } }",
        );
        assert_eq!(
            markup_of(&doc, pos),
            "*untitled*\n\n`MyStaff` context, also known as `Staff`\n\n---\n\nCustom staff."
        );
    }

    #[test]
    fn hover_stays_quiet_over_a_context_type_with_nothing_to_say() {
        // Neither a description nor an alias: a popup reading "`MyStaff`
        // context" over the `MyStaff` you are looking at is worse than
        // nothing, the same rule an undocumented command follows.
        let (doc, pos) =
            doc_at("\\layout { \\context { \\name MyStaff } }\n{ \\new My|Staff { c } }");
        assert!(hover(&doc, pos).is_none());
    }

    #[test]
    fn hover_over_a_context_instance_names_the_type_it_was_created_as() {
        // What a `\lyricsto` site doesn't say for itself: which kind of
        // context `"vocals"` is.
        let (doc, pos) = doc_at("{ \\new Voice = \"vocals\" { c } \\lyricsto \"voc|als\" { la } }");
        assert_eq!(
            markup_of(&doc, pos),
            "*untitled*\n\n`vocals` — a `Voice` context"
        );
    }

    #[test]
    fn hover_over_the_new_keyword_still_describes_the_command() {
        // The context hover is tried first, but only claims the cursor when
        // it really is on the type argument.
        let (doc, pos) = doc_with_a_declared_context("{ \\n|ew MyStaff { c } }");
        assert!(markup_of(&doc, pos).contains("\\new"));
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

    /// One character's worth of state: the active parameter signature help
    /// reports and the completion labels offered, both read immediately
    /// after that character was inserted.
    struct Typed {
        active_parameter: Option<u32>,
        completions: Vec<String>,
    }

    /// Types `typed` into `doc` one character at a time, starting at
    /// `offset`, through [`Document::apply_change`] — the same incremental,
    /// edit-against-the-previous-tree path a real editor drives, and
    /// deliberately not a fresh parse of the finished text.
    ///
    /// That distinction is the point: [`Document`] reparses incrementally
    /// against its old tree on every keystroke (`document.rs`'s
    /// `apply_change`), and that path can behave differently from parsing
    /// the same final text from scratch — an `ERROR`-recovered node from
    /// three keystrokes ago doesn't necessarily resolve itself the same way
    /// a fresh parse of the finished string would. A test built from
    /// [`doc_at`] alone (single parse, cursor dropped in after the fact)
    /// cannot see that difference; only replaying the keystrokes can. The
    /// signature-help/completion flicker reported while typing `\new` is
    /// exactly this: real, only visible mid-edit.
    ///
    /// Returns one [`Typed`] per character of `typed`, so a caller can
    /// inspect the state after any prefix has been typed — in particular
    /// the state immediately before and after each space, which is what the
    /// tests below check.
    fn type_and_record(mut doc: Document, mut offset: usize, typed: &str) -> Vec<Typed> {
        use tower_lsp::lsp_types::TextDocumentContentChangeEvent;

        typed
            .chars()
            .map(|ch| {
                let pos = doc.line_index().position_at(offset);
                doc.apply_change(TextDocumentContentChangeEvent {
                    range: Some(Range::new(pos, pos)),
                    range_length: None,
                    text: ch.to_string(),
                });
                offset += ch.len_utf8();
                let cursor = doc.line_index().position_at(offset);
                Typed {
                    active_parameter: signature_help(&doc, cursor).and_then(|h| h.active_parameter),
                    completions: labels_at(&doc, cursor, &no_install(&doc)),
                }
            })
            .collect()
    }

    /// The [`Typed`] state right before and right after the first space
    /// following `marker` in the string [`type_and_record`] typed —
    /// `marker` being the token that precedes the whitespace boundary a test
    /// wants to check, e.g. `"\\new"` for the space between `\new` and
    /// whatever context type follows it. `marker` must appear in the typed
    /// string with exactly one space right after it, on pain of a panic
    /// that names what went wrong rather than an out-of-bounds index.
    fn gap<'a>(trace: &'a [Typed], typed: &str, marker: &str) -> (&'a Typed, &'a Typed) {
        let after_marker = typed
            .find(marker)
            .unwrap_or_else(|| panic!("{marker:?} not found in {typed:?}"))
            + marker.len();
        assert_eq!(
            typed.as_bytes().get(after_marker),
            Some(&b' '),
            "{marker:?} in {typed:?} must be followed by a single space"
        );
        (&trace[after_marker - 1], &trace[after_marker])
    }

    /// What [`assert_state`] expects [`Typed::completions`] to look like at
    /// one checkpoint — not just whether the list is empty, but which
    /// candidate source it should have come from, so a passing test proves
    /// the right *kind* of completion was offered, not merely a non-empty
    /// one.
    enum Completions {
        /// No candidates — an open-ended parameter (`with`, `music`) or a
        /// parameter with a closed set that happens to have none typed yet.
        None,
        /// `label` is among the candidates offered, from whichever source
        /// (context types, context instances, …) is active at that point.
        Contains(&'static str),
        /// The generic every-command-in-scope fallback
        /// ([`command_names`]/[`word_being_typed`]) — what a cursor mid-word
        /// gets when nothing more specific claims the position first. Typing
        /// an engraver name inside `\with { … }` lands here for want of a
        /// dedicated with-block completion source; that's a known gap of
        /// its own, not part of what this test pins down, so it's only
        /// checked for shape (non-empty, contains an ordinary command) not
        /// content.
        VocabularyFallback,
    }

    /// Checks both halves of [`Typed`] against what a checkpoint should show:
    /// the active parameter signature help reports, and — per `expect` —
    /// what [`completions`] offers there. `label` names the checkpoint in
    /// any failure message.
    fn assert_state(
        state: &Typed,
        active_parameter: Option<u32>,
        expect: Completions,
        label: &str,
    ) {
        assert_eq!(
            state.active_parameter, active_parameter,
            "{label}: active parameter"
        );
        match expect {
            Completions::None => assert!(
                state.completions.is_empty(),
                "{label}: expected no completions, got {:?}",
                state.completions
            ),
            Completions::Contains(want) => assert!(
                state.completions.iter().any(|got| got == want),
                "{label}: expected {want:?} among completions, got {:?}",
                state.completions
            ),
            Completions::VocabularyFallback => assert!(
                state.completions.iter().any(|got| got == "\\relative"),
                "{label}: expected the whole-vocabulary fallback (containing \\relative), got {:?}",
                state.completions
            ),
        }
    }

    /// A document with a declared `MyStaff` context type and one existing
    /// `Voice` instance named `"existing"` — the same no-install-needed
    /// setup [`doc_with_a_declared_context`] uses for hover, extended with
    /// an instance so the `[= name]` parameter's completions
    /// (`context_instance_candidates`) have something to offer besides the
    /// call being typed. The cursor sits inside an already-open `{ }` block
    /// — the ordinary place to start typing a `\new`.
    fn doc_ready_for_new() -> (Document, usize) {
        let text = "\\layout { \\context { \\name MyStaff } }\n\
             { \\new Voice = \"existing\" { c } }\n\
             { }\n"
            .to_string();
        let offset = text.rfind("{ }").unwrap() + 2; // between "{ " and "}"
        (Document::new(text), offset)
    }

    /// A bare `\new` with nothing typed after it yet parses into an `ERROR`
    /// node ([`context_argument_completions`]'s doc explains why), which
    /// used to lose the call for [`signature_help`] entirely — nothing was
    /// reported active at all, rather than the `type` parameter (index 0)
    /// [`context_argument_completions`] was already, separately, offering
    /// context-type completions for at this exact spot. Fixed by
    /// `note_analyser::Analyser::walk`'s `"ERROR"` arm, which recurses into
    /// an error-recovered node instead of skipping it, so `handle_command`
    /// still finds and records the call inside.
    #[test]
    fn signature_help_tracks_the_type_parameter_at_a_bare_new() {
        let typed = "\\new MyStaff";
        let (doc, offset) = doc_ready_for_new();
        let trace = type_and_record(doc, offset, typed);
        let (before, after) = gap(&trace, typed, "\\new");

        assert_state(
            before,
            Some(0),
            Completions::Contains("MyStaff"),
            "before the space after \\new",
        );
        assert_state(
            after,
            Some(0),
            Completions::Contains("MyStaff"),
            "after the space after \\new",
        );
    }

    /// Every combination of `\new`'s two optional pieces — `[= name]` and
    /// `[\with { … }]` — typed out in full, checking both [`signature_help`]'s
    /// active parameter and what [`completions`] offers at every whitespace
    /// boundary in the call's header (`music`'s own body is excluded:
    /// [`Commands::call_site_at`] is documented to report nothing once the
    /// cursor is inside an already-open music body, which is intentional,
    /// not a gap this test is about).
    ///
    /// Parameter indices throughout: 0 = `type`, 1 = `[= name]`, 2 = `with`,
    /// 3 = `music`.
    mod new_signature_help_through_every_combination {
        use super::*;

        /// Both optional pieces present.
        #[test]
        fn name_and_with() {
            let typed = "\\new MyStaff = \"piano\" \\with { \\consists some_engraver } { music }";
            let (doc, offset) = doc_ready_for_new();
            let trace = type_and_record(doc, offset, typed);

            let (before, after) = gap(&trace, typed, "\\new");
            assert_state(
                before,
                Some(0),
                Completions::Contains("MyStaff"),
                "before \\new's space: type",
            );
            assert_state(
                after,
                Some(0),
                Completions::Contains("MyStaff"),
                "after \\new's space: still type",
            );

            let (before, after) = gap(&trace, typed, "MyStaff");
            assert_state(
                before,
                Some(0),
                Completions::Contains("MyStaff"),
                "before the space after the type: still type",
            );
            assert_state(
                after,
                Some(1),
                Completions::Contains("\"existing\""),
                "after the space after the type: = name",
            );

            // `[= name]` is one `ArgKind::Group`, matched prefix-preserving
            // (see `consume_group`): `=` alone, with nothing typed after it,
            // already counts as a complete one-piece group, so once the
            // cursor moves past the trailing whitespace it falls straight
            // through to `with` (index 2) rather than staying on `[= name]`
            // until an actual name follows — a quirk of the same shape as
            // the other two this test pins, but pre-existing and out of
            // scope here, so this only records it, rather than trying to
            // fix it too.
            let (before, after) = gap(&trace, typed, "=");
            assert_state(
                before,
                Some(1),
                Completions::Contains("\"existing\""),
                "before the space after =: = name",
            );
            assert_state(
                after,
                Some(2),
                Completions::None,
                "after the space after =: with, even with no name typed yet",
            );

            let (before, after) = gap(&trace, typed, "\"piano\"");
            assert_state(
                before,
                Some(1),
                Completions::Contains("\"existing\""),
                "before the space after the name: still = name",
            );
            assert_state(
                after,
                Some(2),
                Completions::None,
                "after the space after the name: with",
            );

            // The cursor sits right at the end of the word `\with` itself
            // here, the same "mid-word" spot `\consists` is caught in
            // below — so it gets the same vocabulary fallback, not `with`'s
            // own (empty) candidate list.
            let (before, after) = gap(&trace, typed, "\\with");
            assert_state(
                before,
                Some(2),
                Completions::VocabularyFallback,
                "before the space after \\with: with",
            );
            assert_state(
                after,
                Some(2),
                Completions::None,
                "after the space after \\with: still with",
            );

            // While the `\with` block is open and not yet closed, the whole
            // call sits inside an `ERROR`-recovered node — the same shape as
            // the bare-`\new` dead spot above, and fixed the same way — so
            // it stays on `with` throughout the block's own body rather than
            // going dark.
            let (before, after) = gap(&trace, typed, "{");
            assert_state(
                before,
                Some(2),
                Completions::None,
                "before the space after \\with's opening brace: with",
            );
            assert_state(
                after,
                Some(2),
                Completions::None,
                "after the space after \\with's opening brace: still with",
            );

            let (before, after) = gap(&trace, typed, "\\consists");
            assert_state(
                before,
                Some(2),
                Completions::VocabularyFallback,
                "before the space after \\consists, inside \\with: with",
            );
            assert_state(
                after,
                Some(2),
                Completions::None,
                "after the space after \\consists, inside \\with: still with",
            );

            let (before, after) = gap(&trace, typed, "some_engraver");
            assert_state(
                before,
                Some(2),
                Completions::None,
                "before the space after the engraver name, inside \\with: with",
            );
            assert_state(
                after,
                Some(2),
                Completions::None,
                "after the space after the engraver name, inside \\with: still with",
            );

            let (before, after) = gap(&trace, typed, "}");
            assert_state(
                before,
                Some(2),
                Completions::None,
                "before the space after \\with's closing brace: with",
            );
            assert_state(
                after,
                Some(3),
                Completions::None,
                "after the space after \\with's closing brace: music",
            );
        }

        /// `[= name]` present, `[\with]` never typed at all: signature help
        /// cannot know in advance that `\with` is about to be skipped, so
        /// once the name is complete the next parameter it reports is still
        /// `with` (index 2) — the next one in declaration order — not
        /// `music` (index 3), even though that is what actually follows.
        #[test]
        fn name_only() {
            let typed = "\\new MyStaff = \"piano\" { music }";
            let (doc, offset) = doc_ready_for_new();
            let trace = type_and_record(doc, offset, typed);

            let (before, after) = gap(&trace, typed, "\"piano\"");
            assert_state(
                before,
                Some(1),
                Completions::Contains("\"existing\""),
                "before the space after the name: = name",
            );
            assert_state(
                after,
                Some(2),
                Completions::None,
                "after the space after the name, with never typed: with",
            );
        }

        /// `[\with]` present, `[= name]` skipped.
        #[test]
        fn with_only() {
            let typed = "\\new MyStaff \\with { \\consists some_engraver } { music }";
            let (doc, offset) = doc_ready_for_new();
            let trace = type_and_record(doc, offset, typed);

            let (before, after) = gap(&trace, typed, "MyStaff");
            assert_state(
                before,
                Some(0),
                Completions::Contains("MyStaff"),
                "before the space after the type: still type",
            );
            assert_state(
                after,
                Some(1),
                Completions::Contains("\"existing\""),
                "after the space after the type: = name (still offered, even though about to be skipped)",
            );

            let (before, after) = gap(&trace, typed, "\\with");
            assert_state(
                before,
                Some(2),
                Completions::VocabularyFallback,
                "before the space after \\with, name skipped: with",
            );
            assert_state(
                after,
                Some(2),
                Completions::None,
                "after the space after \\with, name skipped: still with",
            );

            let (before, after) = gap(&trace, typed, "}");
            assert_state(
                before,
                Some(2),
                Completions::None,
                "before the space after \\with's closing brace, name skipped: with",
            );
            assert_state(
                after,
                Some(3),
                Completions::None,
                "after the space after \\with's closing brace, name skipped: music",
            );
        }

        /// Neither optional piece present.
        #[test]
        fn neither() {
            let typed = "\\new MyStaff { music }";
            let (doc, offset) = doc_ready_for_new();
            let trace = type_and_record(doc, offset, typed);

            let (before, after) = gap(&trace, typed, "MyStaff");
            assert_state(
                before,
                Some(0),
                Completions::Contains("MyStaff"),
                "before the space after the type: still type",
            );
            assert_state(
                after,
                Some(1),
                Completions::Contains("\"existing\""),
                "after the space after the type: = name (still offered, even though about to be skipped)",
            );
        }
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

    #[test]
    fn signature_help_clears_its_active_parameter_right_past_a_closed_string() {
        // A quoted string's span already ends at its own closing quote, so
        // typing (or just moving the cursor) one more byte past it can never
        // still be extending `filename` — unlike a bareword or number, which
        // has nothing stopping it from growing right there. Both offsets
        // sit in the trailing whitespace `covers` bridges (see
        // `call_site_just_past_a_half_typed_argument` in `command::mod`), so
        // getting this right is `align_arg_to_param`'s job, not `covers`'.
        let (doc, pos) = doc_at("\\include \"foo.ly\"|");
        let help = signature_help(&doc, pos).expect("signature help");
        assert_eq!(help.active_parameter, None);

        let (doc, pos) = doc_at("\\include \"foo.ly\" |");
        let help = signature_help(&doc, pos).expect("signature help");
        assert_eq!(help.active_parameter, None);
    }
}
