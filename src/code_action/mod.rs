//! Code actions the server offers for a selection: refactorings and quick
//! fixes the editor lists in its lightbulb menu.
//!
//! Each action lives in its own submodule and implements [`CodeAction`]. The
//! work is split in two so listing the menu stays cheap no matter how many
//! actions exist: [`offer`](CodeAction::offer) only decides whether the action
//! applies and what to call it, while [`resolve`](CodeAction::resolve) builds
//! the edits and runs lazily — through `codeAction/resolve` — only for the
//! action the user actually picks. The selection (and the document it referred
//! to) is
//! carried across the two requests in the action's opaque [`data`] field.
//!
//! [`data`]: tower_lsp::lsp_types::CodeAction::data

pub mod extract_to_variable;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{
    CodeAction as LspCodeAction, CodeActionKind, CodeActionOrCommand, Command, Range, TextEdit,
    Url, WorkspaceEdit,
};

use crate::document::Document;
use extract_to_variable::ExtractToVariable;

/// A code action the server can offer for a selection in a document.
pub trait CodeAction {
    /// Distinguishes this action's [`resolve`] payloads from other actions'.
    const ID: &'static str;

    /// Cheaply decides whether this action applies to `selection`, returning
    /// the menu entry to show or `None` to stay out of the menu. Builds no
    /// edits: it runs for every action on every code-action request.
    fn offer(document: &Document, selection: Range) -> Option<Offer>;

    /// Builds the edits that perform the action. Runs only for the action the
    /// user picks (through `codeAction/resolve`), so this is where the expensive
    /// work belongs. `None` if the document no longer supports the action (e.g.
    /// it changed since the menu was shown).
    fn resolve(document: &Document, selection: Range) -> Option<Resolved>;
}

/// An action's menu entry: how it is labelled and categorised in the editor.
pub struct Offer {
    pub title: String,
    pub kind: CodeActionKind,
}

/// The edits that perform an action, plus an optional follow-up command.
pub struct Resolved {
    pub edits: Vec<TextEdit>,
    pub command: Option<Command>,
}

/// Carried in an offered action's `data` so `resolve` can rebuild it: which
/// action it was, and the document and selection it applied to.
#[derive(Serialize, Deserialize)]
struct ResolveData {
    id: String,
    uri: Url,
    selection: Range,
}

/// Offers every code action that applies to `selection`, as unresolved actions
/// (title and kind only — the edits come later, from [`resolve`]).
pub fn offer_all(document: &Document, uri: &Url, selection: Range) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();
    offer::<ExtractToVariable>(document, uri, selection, &mut actions);
    actions
}

/// Offers action `A` if it applies, pushing an unresolved LSP action onto `out`.
fn offer<A: CodeAction>(
    document: &Document,
    uri: &Url,
    selection: Range,
    out: &mut Vec<CodeActionOrCommand>,
) {
    if let Some(offered) = A::offer(document, selection) {
        let data = ResolveData {
            id: A::ID.to_string(),
            uri: uri.clone(),
            selection,
        };
        out.push(CodeActionOrCommand::CodeAction(LspCodeAction {
            title: offered.title,
            kind: Some(offered.kind),
            data: Some(serde_json::to_value(data).expect("resolve data serialises")),
            ..LspCodeAction::default()
        }));
    }
}

/// The document an offered `action` applies to, read back from its `data`.
/// Lets the caller pick the right document before [`resolve`] rebuilds the edit.
pub fn target_uri(action: &LspCodeAction) -> Option<Url> {
    let data = action.data.clone()?;
    serde_json::from_value::<ResolveData>(data)
        .ok()
        .map(|d| d.uri)
}

/// Fills in the edits for a previously offered `action` by re-running the
/// matching action's [`resolve`](CodeAction::resolve) against `document`.
/// Returns the action unchanged if its `data` is missing, unrecognised, or no
/// longer applies.
pub fn resolve(document: &Document, mut action: LspCodeAction) -> LspCodeAction {
    let Some(data) = action.data.take() else {
        return action;
    };
    let Ok(data) = serde_json::from_value::<ResolveData>(data) else {
        return action;
    };

    let resolved = match data.id.as_str() {
        ExtractToVariable::ID => ExtractToVariable::resolve(document, data.selection),
        _ => None,
    };

    if let Some(resolved) = resolved {
        let mut changes = HashMap::new();
        changes.insert(data.uri, resolved.edits);
        action.edit = Some(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        });
        action.command = resolved.command;
    }
    action
}
