# Command knowledge

The server needs to understand the *arguments* to commands, not just recognise their names. `\repeat volta 3 { … }` should offer `volta` as a completion, highlight it as a keyword even in lyric mode, prompt with the argument list as you type it, and hover with useful prose. Today that knowledge is spread across two places that don't know about each other: the `SPECS` table in [`src/command.rs`](../src/command.rs), which describes three commands well enough for the repeat refactorings, and `Analyser::handle_command` in [`src/note_analyser.rs`](../src/note_analyser.rs), which re-derives the argument shapes of a dozen more by hand so the note pass can skip over them.

This document describes the design that replaces both, and the order to build it in.

## What it buys

- **Completion** inside an argument (`volta`, `unfold`, `percent`, `tremolo` after `\repeat`).
- **Signature help** — `textDocument/signatureHelp`, argument-position-aware, as the user types.
- **Semantic highlighting** of bare-word arguments, which the TextMate grammar can't get right because it doesn't know which words are arguments to what.
- **Hover** with per-command and per-argument documentation.
- **Diagnostics** for wrong arity and wrong argument shape, where LilyPond's own messages are famously opaque.
- **Correct refactoring boundaries.** Extract-to-variable and inline currently guess at commands `SPECS` doesn't cover; a general table stops the guessing.
- **A single implementation of argument skipping** for the note analyser, in place of `handle_command`'s hand-rolled index arithmetic.

## Where the knowledge comes from

Three layers, resolved in priority order. This split is not a hedge — it follows a seam in LilyPond itself.

| Layer | Source | Status |
|---|---|---|
| `builtin` | Hand-written impls in this repo | Built (step 1) |
| `workspace` | Definitions parsed from the user's open and `\include`d files | Built (step 3) |
| `install` | `define-music-function` and friends read out of the active LilyPond install | Later |

`workspace` outranks `install` because a user redefining `\foo` means theirs. `builtin` outranks both, because it exists precisely where the other two are absent or unhelpful.

**The keyword layer can only ever be hand-written.** `\repeat` is a reserved word in LilyPond's Bison grammar; the `repeat` in `ly-syntax-constructor.scm` is the constructor the parser calls, not a function reachable as `\repeat`. The same holds for `\context`, `\new`, `\override`, `\set`, `\with`, `\alternative`, `\change` and the mode-switching commands. No amount of Scheme reading will produce them. Conveniently this set is small, changes rarely between LilyPond versions, and is exactly the set whose documentation is worth tailoring by hand.

Conversely the ~400 music functions are not worth hand-writing, and hand-written entries can never cover user-defined functions at all. Those come from the later layers.

### Decisions already taken for the later layers

Recorded before those layers were built, because they shaped the trait.

- **Read, don't evaluate.** Recognising `(define-music-function (a b) (pred? pred?) "doc" …)` is a datum-shape match, not a computation. Reading avoids embedding a Scheme interpreter, and avoids executing workspace-authored code in the server process.
- **If evaluation ever becomes unavoidable, shell out to the user's own `lilypond`** rather than embedding an interpreter: run it once over a generated `.ly` that dumps every function's name, `ly:music-function-signature` and docstring, and cache the result keyed on the binary's path and mtime. This is the same data the manuals are generated from. It requires respecting VS Code's workspace trust, since it executes workspace-reachable code.
- **Match on the definition form, not on `define-public`.** Harvesting `define-public` fabricates commands that aren't callable as `\foo`. The reliable markers are `define-music-function` / `define-event-function` / `define-scheme-function` / `define-void-function` in value position, plus `foo = #(define-… )` in `ly/*.ly`.
- **Docstrings arrive as Texinfo** (`@var{}`, `@code{}`, wrapped in `_i` for gettext). Convert to Markdown once on the way in, not per hover.

## The design

The `Command` trait and supporting types can be found in src/command/mod.rs.

Code actions are deliberately not a method here. `code_action` already owns an offer/resolve lifecycle (see [`src/code_action/README.md`](../src/code_action/README.md)); an action that wants command knowledge asks the table for it, keeping the dependency one-way.

### Supporting types

### The table

The symbol table is [`vocabulary.rs`](../src/vocabulary.rs), which already answered "is `\foo` a command?" and now answers "…and what does it do?". Reusing it avoids a second registry and a second name; `is_known` becomes a thin wrapper over lookup plus the existing CamelCase context-reference rule.

Step 1 wrote this as one flat struct with a field per source. Step 3 made the middle field *per file* rather than per workspace, which turns the whole thing into a scoped symbol table: each source of knowledge is a `Layer`, and what a document sees is a stack of them.

A bare `known_names` entry resolves to no `Command` at all, rather than to a placeholder with an empty signature. Knowing that a name exists says nothing about its arguments, and `command::parse` declining the call is exactly what leaves the block after `\break` to be read as ordinary music.

**A definition is a command.** `foo = { c d e }` binds a name that `\foo` substitutes and that consumes nothing after it — a zero-argument command. So there is no separate notion of a definition: a file's `Layer` holds everything it binds, whether or not anything says what arguments it takes, and one lookup answers both "where is `\foo` defined?" and "what does `\foo` do?". That collapses what used to be two walks of the include closure (one for scopes, one for a flat set of reachable definition names) into one, and gives variables the nearest-wins shadowing a flat set can't express. The line that remains is not definition-versus-command but *we have a definition for it* versus *we have only ever heard the name*: a `lilypond-words` entry like `\break` still resolves to nothing, which is what keeps the block after it read as ordinary music.

An entry carries the span of the name as written, and — where a file binds the same name more than once — a chain to the definition it replaced (`Command::redefines`). LilyPond takes the last binding, so that is the one the layer hands out. Keeping the replaced *command* rather than a flat list of spans is what a redefinition warning or code lens would read.

**A redefinition replaces from the point it appears, and no earlier.** LilyPond substitutes a variable where it is used, so a `\foo` written between two definitions of `foo` means the first one. `Document::definition_in_effect` walks the chain and takes the last binding at or before the reference, which is what go-to-definition resolves to: one definition, the one that reference actually means, not every place the name was ever bound. A reference in an *including* file passes no position — an `\include` is textually substituted, so the whole included file precedes it and its last binding wins.

Rename and find-references deliberately don't do this. They treat a name as one thing across a file, because renaming the second `foo` while leaving the first alone would rewrite the references that meant the first one too. Resolving each reference to its own binding before rewriting is a scoped rename, and a bigger job. Note analysis doesn't do it either: `\foo` is parsed against the layer's entry, so a call written before a redefinition is read with the *later* signature. That only shows if a name is bound to functions of different arities in one file, which no real score has yet done to us.

Three consequences of the layering, because they are what make the cross-file part work:

- **A file's definitions are read once, when its `Document` is parsed.** The layer lives on the `Document`; every scope that reaches the file shares the same `Arc<Layer>`. A header included by twenty scores is read once, not twenty times.
- **A scope is compared by its layers' identities, not their contents.** Editing a file mints a new layer id, so every scope containing it fingerprints differently and `Document::refresh` re-analyses it on the next query. That needs no reverse include index and no eager invalidation walk at the moment of the edit; the cost falls only on the documents actually asked about.
- **Empty layers are skipped.** A file that binds nothing, and includes nothing that does, fingerprints the same as the bare builtin scope. This used to spare almost every document a second analysis; now that a plain `foo = { c }` puts an entry in the layer it spares rather fewer, and editing a melody include re-analyses the scores that include it. The cost is one extra analysis of a document the next time it is asked about — lazily, as ever. If it ever bites, the fix is to derive the layer id from a hash of the parse-relevant content (names and signatures, *not* spans), so that editing a definition's body invalidates nothing.

Note that `command::Commands` is a different thing with a confusingly close name: it is the list of `CommandCall`s *found in one document*, not the table of commands that exist. Leave it be for now; renaming it to `CommandCalls` is a tidy-up worth doing separately.

## How the note analyser uses it

This is the part that makes step 1 worth doing on its own, and the part most likely to be got wrong.

`Analyser::handle_command` currently does two jobs at once: skipping over a command's arguments so they aren't misread as notes, and deciding what mode and region the command's body is read in. The first job is exactly `parse_args`. The second is exactly `music_context`. So `handle_command` collapses to:

1. Look the `escaped_word` up in the document's `Scope`.
2. `parse_args` to build the `CommandCall` — this consumes the reference pitch, the clef name, the property path, the repeat kind, replacing every hand-written `children.get(i).kind() == "symbol"` check in that function.
3. Ask `music_context` what the body is read in.
4. Walk each `Arg::Music` in that context.
5. Return `ArgReader::position()` as the next index.

The mode/region logic moves out of the `match` on command name and into the impls: `Relative` returns `MusicContext::Relative(pitch)` from its own parsed `Arg::Pitch`; `ChordMode` returns `Chord`; `LyricMode`, `Markup`, `Header`, `Paper`, `Layout`, `Midi`, `With` return `NonNote`. The analyser stops knowing any command names at all.

Two behaviours must be preserved exactly, because tests depend on them:

- **A command with no signature still lets its following block be read as music by the main loop.** That is what the current `_ => start + 1` arm does, and it must remain true for names known only from `lilypond-words`.
- **`after_event` must still be cleared** after a command, so a bare integer following `\volta 1` isn't read as a bare duration.

## Implementation order

**Step 0 — pin the existing bugs with failing tests.** Step 1 fixes two mis-readings that `note_analyser.rs`'s "Known limitations" section documents: the bare-symbol arguments of `\key` and `\transpose` are read as spurious note events, which also perturbs the running `\relative` reference. Write those tests first, marked `#[ignore]` with a comment pointing at step 1, so the fix is demonstrated rather than asserted. Also add a test that `\tempo`'s duration argument doesn't affect the duration of the following note.

The cleanest form is *differential*: assert that analysing a snippet gives the same events as analysing it with the offending command and its arguments removed. That sidesteps having to write out expected octave numbers, and states the property we actually want — that a command's arguments contribute no events and leave the relative reference untouched.

| Case | Wrong today |
|---|---|
| `{ \key g \major a4 b }` | Three events; the `g` is read as a note. Should be two. |
| `\relative c' { \key g \major a4 }` | `a4` resolves against `g`, not against `c'`. Should match `\relative c' { a4 }`. |
| `{ \transpose c d { e4 } }` | Three events. Should be one. |
| `\relative c' { \transpose c d { g4 } }` | Reference moved twice before the body. Should match `\relative c' { { g4 } }`. |
| `{ \key cis \minor c4 }` | The tonic's accidental-bearing name is read as a note too. Should be one event. |

Every command that gains a signature in step 1 gets a case here, not just the two with documented bugs — "no bug is known" isn't evidence that the behaviour is right, and these tests are what makes step 1's behaviour-preserving claim checkable rather than asserted.

One existing test has to change rather than be added: `command.rs`'s `an_unregistered_command_is_not_parsed` asserts that `\transpose c d { e }` is *declined* by the parser. Once `\transpose` has a signature that is no longer true, so replace it with a case using a command that genuinely has no entry.

### Go-to-definition for user-defined music functions

A music function in a user's file is written as an ordinary assignment, `myFunc = #(define-music-function …)`, and `document.rs`'s symbol query captures a definition from the left-hand side alone:

```
(lilypond_program (assignment_lhs (symbol) @definition))
```

Nothing there looks at the right-hand side, so this is expected to work already, for go-to-definition, find-references, and the undefined-reference diagnostic alike. Expected, but unverified — add tests now so that step 3 can't quietly break it:

| Case | Expected |
|---|---|
| `myFunc = #(define-music-function (m) (ly:music?) m)` then `\myFunc { c4 }` | `\myFunc` resolves to the assignment; no undefined-reference diagnostic |
| The same definition in an `\include`d file | Resolves across the include graph, like any other variable |
| `\myFunc` with no definition anywhere | Still diagnosed as undefined |

**Step 1 — the trait, hand-written commands, and every call site.** No Scheme, no new layers, no new LSP features.

1. Add the trait and supporting types to `src/command.rs`, with `default_parse` driving off `signature()`. Implement the new `ArgKind` variants in `ArgReader::take`.
2. Write `builtin` impls for the commands currently hard-coded in the two places: `\repeat`, `\volta`, `\alternative` (from `SPECS`), and `\relative`, `\fixed`, `\notemode`, `\chordmode`, `\drummode`, `\figuremode`, `\lyricmode`, `\addlyrics`, `\lyricsto`, `\markup`, `\markuplist`, `\header`, `\paper`, `\layout`, `\midi`, `\with`, `\clef`, `\set`, `\unset`, `\language`, `\include` (from `handle_command`), including their short-form aliases (`\chords`, `\lyrics`, `\notes`, `\drums`, `\figures`).
3. Delete `SPECS` and `spec_for`; point `command::parse` at the table.
4. Rewrite `handle_command` as the five steps above and delete the per-command `match`.
5. Extend `Vocabulary` with the layered structure, with only `builtin` and `known_names` populated.
6. Add `\key`, `\transpose`, and `\tempo`, whose arguments the analyser currently mis-reads, turning the step 0 tests green. Drop their entry from the "Known limitations" list in `note_analyser.rs`'s module docs.

Apart from item 6 this step is behaviour-preserving: the existing suites must pass unchanged.

**Step 2 — new LSP features off the table.** Signature help, argument completion, hover, semantic tokens for bare-word arguments, and `check`-based diagnostics. Each is independent; none needs the later layers.

**Step 3 — the workspace layer. Done.** `define-music-function` and friends are read out of the user's own files by [`src/command/scheme.rs`](../src/command/scheme.rs), into one `Layer` per file, stacked into a `Scope` by [`document_graph.rs`](../src/document_graph.rs) along the include closure. Cross-file invalidation landed as described in "The table" above: the analysis records the scope's fingerprint, and `Document::refresh` redoes it when that changes. The architectural cost was real but smaller than feared, for two reasons — the LilyPond grammar already parses embedded Scheme (no second grammar), and keying the analysis on the scope removed the need to invalidate dependants eagerly.

What it reads, and what it deliberately doesn't:

- Both naming shapes: `myFunc = #(define-…-function …)` and `#(define-public myFunc (define-…-function …))`. All four `define-…-function` forms count, since all four are called the same way.
- Signatures are aligned with the predicate list **from the right**, so the pre-2.15 `(parser location note)` argument lists still in use across real libraries read as one argument, not three.
- A predicate becomes an `ArgKind` only where its source *shape* is known (`ly:music?`, `ly:pitch?`, `string?`, the integer ones); everything else is `ArgKind::Unknown(predicate)`, consuming one node. So an unfamiliar predicate costs the extent of one argument, not the whole signature.
- Docstrings are converted from Texinfo to Markdown once, on the way in, `(_i "…")` wrappers included.
- Function *bodies* are not read at all, `#{ … #}` least of all: that is what keeps the two readers from becoming mutually recursive.

Two things fell out, as expected. Signatures come from the right-hand side of an assignment the symbol query already sees, so the step 0 go-to-definition tests are the guard that the two views of the same definition stay consistent. And a function defined only inside `#( … )` stopped being a false-positive undefined reference — and then, since the reader knows *where* each name was written and not merely that it exists, stopped being a second-class definition altogether: go-to-definition, find-references and rename all reach it. See "Go-to-definition for user-defined music functions" above.

**Step 3a — definitions become commands.** The two readers now produce `Binding`s rather than a layer and a separate list of symbols, and [`src/command/definition.rs`](../src/command/definition.rs) merges them into the file's one table, as "A definition is a command" above describes. That module owns what a *file* defines: `Binding`, the `Definition` decorator that gives any command the place its file wrote the name, and the `Variable` a binding becomes when nothing says what arguments it takes. One guard had to land with it: `command_assist::hover` returns `None` for a command with neither parameters nor prose, because otherwise every `\foo` reference to a plain variable pops up a box containing just `\foo`. Rendering a variable's *value* on hover is the obvious thing to do with that space, and is not done yet.

**Step 4 — the install layer.** The same reader pointed at the active install's `.scm` and `ly/*.ly`, indexed asynchronously on startup and cached on disk keyed on an install fingerprint, so first diagnostics aren't blocked.

## Constraints to respect

- **Optional arguments are genuinely ambiguous.** LilyPond's parser runs predicates on parsed values and backtracks (`\tweak`, `\override`, `\shape`, `\footnote`). Shape-matching gets most of the way; where it can't, hand-write the impl rather than complicating `default_parse`.
- **The maintenance surface of the later layers is predicates, not functions** — about 30 predicates in real use against ~400 functions, and predicates change far more slowly. `ArgKind::Unknown` is what keeps one unrecognised predicate from voiding a whole signature.
- **`#{ … #}` makes the two readers mutually recursive.** Signatures alone dodge this. Anything that reads function *bodies* does not, so don't.

## Testing

- Every hand-written command gets unit tests in `src/command.rs` in the style of the existing ones, including a half-typed case that exercises stopping at a missing required argument.
- Step 4 must not make tests depend on an installed LilyPond. It should check in fixture `.scm` and dump files from the start.
