# Code actions

This module holds the refactorings and quick fixes the server offers in the
editor's lightbulb menu. Each action lives in its own submodule and implements
the [`CodeAction`](mod.rs) trait; [`extract_to_variable`](extract_to_variable.rs)
is the worked example.

## The two-step lifecycle

Listing the menu must stay cheap however many actions exist, so an action is
produced in two steps that map onto two LSP requests:

| Step | Trait method | LSP request | Cost |
|------|--------------|-------------|------|
| Decide whether to offer it, and how to label it | `offer` | `textDocument/codeAction` | cheap — runs for *every* action, *every* time |
| Build the edits that perform it | `resolve` | `codeAction/resolve` | expensive — runs only for the action the user picks |

`offer` returns title and kind and nothing else: **no edits are built when the
menu is shown**. When the user picks an action, the editor sends a resolve
request and only then does `resolve` run, for that one action.

## Carrying context across the two requests

`offer` and `resolve` are both stateless `(document, selection)` functions —
they share no instance. The resolve request, however, arrives carrying only the
action object, not a position. We bridge the gap through the action's opaque
[`data`] field: when offering an action we stash its `ID`, the document `uri`,
and the `selection` there (see `ResolveData` in [mod.rs](mod.rs)). On resolve we
read them back, look up the document, and dispatch to the matching action's
`resolve`.

Because the document may have changed between the menu appearing and the click,
`resolve` re-derives everything from the current document rather than trusting
anything precomputed. For `extract_to_variable` that means re-running its
validity check — which is also why that check lives in a shared `validate`
helper rather than being duplicated across `offer` and `resolve`.

[`data`]: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#codeAction

## Where to draw the line between `offer` and `resolve`

`offer` decides whether the action is *available*; `resolve` produces the edits.
A menu must never list an action that then does nothing, so `offer` has to be a
faithful predicate — it offers exactly the selections `resolve` can carry out.
The split is therefore not "cheap validation vs. full validation" but
**validation vs. construction**: `offer` runs the whole availability check,
while everything whose only purpose is to *build the edits* is deferred to
`resolve`.

Extract-to-variable factors this into a shared `validate`. It does the full
availability check — in bounds, parses cleanly, doesn't slice through a single
symbol or a `{ }` block, doesn't cut a note at either edge — and `offer` is
nothing more than `validate(...).is_some()`. What `resolve` adds is the work
that only shapes the result: resolving the wrapping *mode* (which walks the
block ancestry), making durations and octaves explicit, and assembling the text
edits. None of that can change whether the action applies, so none of it is
paid while the menu is merely being listed.

The guiding question for a new action: would this computation ever change the
*yes/no* of whether to offer? If yes, it belongs in the availability check that
`offer` runs. If it only affects *what the edit looks like*, defer it to
`resolve`.

## Adding an action

1. Add a submodule and a unit type implementing `CodeAction`, with a unique
   `ID`.
2. Register it in `offer_all` (one `offer::<YourAction>(…)` line) and in the
   `match` in `resolve`.
3. If it needs to read the parse tree or note analysis, add an accessor to
   `Document` rather than re-parsing — the document is parsed once and shared
   across every action's `offer`.
