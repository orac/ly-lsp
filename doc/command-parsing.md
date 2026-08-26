# Command knowledge

The server understands the *arguments* to commands, not just their names. `\repeat volta 3 { … }` offers `volta` as a completion, highlights it as a keyword even in lyric mode, prompts with the argument list as you type it, and hovers with useful prose. This document describes how that knowledge is represented, where it comes from, and what remains to be built.

## What it buys

- **Completion** of a command name, from everything in scope, each labelled with where it came from — and inside an argument (e.g. `\repeat volta`), which is offered in preference wherever a parameter has a closed set of values.
- **Signature help** — `textDocument/signatureHelp`, argument-position-aware, as the user types.
- **Semantic highlighting** of bare-word arguments, which the TextMate grammar can't get right because it doesn't know which words are arguments to what.
- **Hover** with per-command and per-argument documentation.
- **Diagnostics** for wrong arity and wrong argument shape.
- **Correct refactoring boundaries.** Extract-to-variable and inline know where a command's arguments end rather than guessing.
- **A single implementation of argument skipping** for the note analyser, in place of hand-rolled index arithmetic.

## Where the knowledge comes from

Three kinds of layers, resolved in priority order.

| Layer | Source | Status |
|---|---|---|
| `builtin` | Hand-written impls in this repo | Built |
| `workspace` | Definitions parsed from the user's open and `\include`d files | Built |
| `install` | `define-music-function` and friends read out of the active LilyPond install | Built |

`workspace` outranks `install` because a user redefining `\foo` means theirs. `builtin` outranks both, because it exists precisely where the other two are absent or unhelpful.

**The keyword layer can only ever be hand-written.** `\repeat` is a reserved word in LilyPond's Bison grammar; the `repeat` in `ly-syntax-constructor.scm` is the constructor the parser calls, not a function reachable as `\repeat`. The same holds for `\context`, `\new`, `\override`, `\set`, `\with`, `\alternative`, `\change` and the mode-switching commands. No amount of Scheme reading will produce them. Conveniently this set is small, changes rarely between LilyPond versions, and is exactly the set whose documentation is worth tailoring by hand.

### Decisions that shaped the trait

- **Parse but don't evaluate Scheme code** Recognising `(define-music-function (a b) (pred? pred?) "doc" …)` is a datum-shape match, not a computation. Reading avoids embedding a Scheme interpreter, and avoids executing workspace-authored code in the server process.
- **If evaluation ever becomes unavoidable, shell out to the user's own `lilypond`** rather than embedding an interpreter: run it once over a generated `.ly` that dumps every function's name, `ly:music-function-signature` and docstring, and cache the result keyed on the binary's path and mtime. This is the same data the manuals are generated from. It requires respecting VS Code's workspace trust, since it executes workspace-reachable code.
- **Match on the definition form, not on `define-public`.** Harvesting `define-public` fabricates commands that aren't callable as `\foo`. The reliable markers are `define-music-function` / `define-event-function` / `define-scheme-function` / `define-void-function` in value position, plus `foo = #(define-… )` in `ly/*.ly`.
- **Docstrings arrive as Texinfo** (`@var{}`, `@code{}`, wrapped in `_i` for gettext). Convert to Markdown once on the way in, not per hover.

## The design

The `Command` trait and supporting types can be found in [`src/command/mod.rs`](../src/command/mod.rs).

Code actions are deliberately not a method there. `code_action` already owns an offer/resolve lifecycle (see [`src/code_action/README.md`](../src/code_action/README.md)); an action that wants command knowledge asks the table for it, keeping the dependency one-way.

### The table

The symbol table is [`vocabulary.rs`](../src/vocabulary.rs), which answers both "is `\foo` a command?" and "…and what does it do?". Reusing one registry for both avoids a second name for the same thing; `is_known` is a lookup against the command namespace, then the context-type namespace, and only then the CamelCase context-reference rule — and that rule is itself a fallback, used only when the scope's context-type namespace is empty everywhere in scope (a failed install), not a rule about what a well-typed context reference looks like. See `Scope::is_known`'s own rustdoc for the reasoning, and [`contexts.md`](contexts.md) for the context-type namespace itself.

Each source of knowledge is a `Layer`, and what a document sees is a `Scope`: a stack of them, top down —

| Layer | Why there |
|---|---|
| `command::RESERVED` | LilyPond's grammar recognises these before any name lookup happens, so nothing can rebind them |
| the document, then its includes, nearest first | a file that defines `\foo` means its own `\foo` |
| `command::CURATED` | our wording and signatures beat what the reader recovers from the install — but these are ordinary functions, and a file may shadow them |
| the install | what LilyPond itself defines |
| the words list | names with nothing behind them |

**Precedence is the order of the stack.** `Scope::get` is a search from the top, and returns the command and which layer it came from; `Scope::get_context_type` does the same for context types. `Scope::is_known` tries both, and falls back to the CamelCase rule (a rule about the *shape* of a name, which no map of names can hold) only when nothing in scope declares a context type at all. Every scope is assembled by `Scope::for_document`, so that function is the single place the order is stated.

**The hand-written table is two layers because it answers two questions.** `\repeat`, `\set` and the mode switches are reserved words in LilyPond's own grammar and cannot be shadowed by declarations in files. `\clef`, `\key`, `\relative` etc. are ordinary `define-music-function`s in `ly/music-functions-init.ly`; we keep curated signatures for them because ours are better than what the reader recovers (`\relative` and `\fixed` most of all, whose octave-reference behaviour no signature expresses), but LilyPond's own lookup lets a file shadow them, so they sit *below* the file layers.

**Every layer says where its knowledge came from**, as a `Layer::origin`: the file's name for a document, `lilypond-2.24.3` for the install, `lilypond-words` for the words list, `built-in` for the two hand-written layers. Hover shows it as its first line, so a reader can tell their own `\foo` from LilyPond's. It sits on the layer rather than on each command because it is a property of the source, one per layer, and every command in a layer shares it. Every layer has one: a buffer with no file behind it is `untitled`, as its URI would be.

**Completing a name is a question for the whole scope**, not for any one command: `Scope::visible` is `get` asked of every name at once, each answered by the nearest layer that has it, so a shadowed `\foo` is never offered alongside the one that would actually be called. Each item's `detail` is that layer's origin, which is what tells two `\foo`s apart in a list where hover can't reach. Because the point is to complete a name that isn't finished, the cursor position is read lexically — the backslash-word being typed — rather than from the parse tree, which has no call there to find; the completion replaces that whole word, backslash included, since the editor's own idea of a word may not include it.

**A command can compute its completions**, through `Command::completions(index, ctx)`. Nearly all of them return a slice of a table written in the source, but `\version` offers the version of the installed LilyPond, which no table here can know: the `CompletionContext` carries it down from `DocumentGraph`, which reads it from the share directory the client named at `initialize`. With no installation behind it, `\version` offers nothing rather than a number that would be a lie.

A words-list entry resolves to a `Variable`: a command with an empty signature. A call to one consumes nothing, so the block after `\break` is read as ordinary music.

**A `Scope` is a persistent list.** Cloning shares every layer and `Scope::extended_with` shares the whole tail, so the three global layers are built once at `initialize` and each document's scope is built *from* that base rather than beside it.

**A definition is a command.** `foo = { c d e }` binds a name that `\foo` substitutes and that consumes nothing after it — a zero-argument command. So there is no separate notion of a definition: a file's `Layer` holds everything it binds, whether or not anything says what arguments it takes, and one lookup answers both "where is `\foo` defined?" and "what does `\foo` do?". That collapses what would otherwise be two walks of the include closure (one for scopes, one for a flat set of reachable definition names) into one, and gives variables the nearest-wins shadowing a flat set can't express. The line is not definition-versus-command but *we have a definition for it* versus *we have only ever heard the name*.

An entry carries the span of the name as written, and — where a file binds the same name more than once — a chain to the definition it replaced (`Command::redefines`). LilyPond takes the last binding, so that is the one the layer hands out. Keeping the replaced *command* rather than a flat list of spans is what a redefinition warning or code lens would read.

**A redefinition replaces from the point it appears, and no earlier.** LilyPond substitutes a variable where it is used, so a `\foo` written between two definitions of `foo` means the first one. `Document::definition_in_effect` walks the chain and takes the last binding at or before the reference, which is what go-to-definition resolves to: one definition, the one that reference actually means, not every place the name was ever bound. A reference in an *including* file passes no position — an `\include` is textually substituted, so the whole included file precedes it and its last binding wins.

Rename and find-references deliberately don't do this. They treat a name as one thing across a file, because renaming the second `foo` while leaving the first alone would rewrite the references that meant the first one too. Resolving each reference to its own binding before rewriting is a scoped rename, and a bigger job. Note analysis doesn't do it either: `\foo` is parsed against the layer's entry, so a call written before a redefinition is read with the *later* signature. That only shows if a name is bound to functions of different arities in one file, which no real score has yet done to us.

Three consequences of the layering, because they are what make the cross-file part work:

- **A file's definitions are read once, when its `Document` is parsed.** The layer lives on the `Document`; every scope that reaches the file shares the same `Arc<Layer>`. A header included by twenty scores is read once, not twenty times.
- **A scope is compared by its layers' identities, not their contents.** Editing a file mints a new layer id, so every scope containing it fingerprints differently and `Document::refresh` re-analyses it on the next query. That needs no reverse include index and no eager invalidation walk at the moment of the edit; the cost falls only on the documents actually asked about.
- **Empty layers are skipped.** A file that binds nothing, and includes nothing that does, fingerprints the same as the bare builtin scope, and is never re-analysed. Now that a plain `foo = { c }` puts an entry in the layer this spares fewer documents than it once did, and editing a melody include re-analyses the scores that include it. The cost is one extra analysis of a document the next time it is asked about — lazily, as ever. If it ever bites, the fix is to derive the layer id from a hash of the parse-relevant content (names and signatures, *not* spans), so that editing a definition's body invalidates nothing.

Note that `command::Commands` is a different thing with a confusingly close name: it is the list of `CommandCall`s *found in one document*, not the table of commands that exist. Renaming it to `CommandCalls` is a tidy-up worth doing separately.

## How the note analyser uses it

`Analyser::handle_command` does two jobs, and the table does both for it: skipping over a command's arguments so they aren't misread as notes is `parse_args`, and deciding what mode and region the command's body is read in is `music_context`. It handles a `named_context` node — `\new Staff`, `\context Voice = "vocals"`, a single node the grammar folds the keyword and its context type into (see "The design" above) — the same way as a bare `escaped_word`: `command::parse` unwraps the shape before `handle_command` ever sees it, so the same table and the same steps below serve both. So it is five steps:

1. Look the `escaped_word` up in the document's `Scope`.
2. `parse_args` to build the `CommandCall` — this consumes the reference pitch, the clef name, the property path, the repeat kind.
3. Ask `music_context` what the body is read in.
4. Walk each `Arg::Music` in that context.
5. Return `ArgReader::position()` as the next index.

The mode and region logic lives in each impl of Command, with no centralised logic in the analyser.

Two behaviours the tests pin down, and which any new layer must preserve:

- **A command with no signature still lets its following block be read as music by the main loop.** That must remain true for names known only from `lilypond-words`.
- **`after_event` must still be cleared** after a command, so a bare integer following `\volta 1` isn't read as a bare duration.

## The workspace layer

`define-music-function` and friends are read out of the user's own files by [`src/command/scheme.rs`](../src/command/scheme.rs), into one `Layer` per file, stacked into a `Scope` by [`document_graph.rs`](../src/document_graph.rs) along the include closure. Cross-file invalidation works as "The table" describes: the analysis records the scope's fingerprint, and `Document::refresh` redoes it when that changes.

Two things made this cheaper than feared: the LilyPond grammar already parses embedded Scheme, so there is no second grammar, and keying the analysis on the scope removed the need to invalidate dependants eagerly.

What it reads, and what it deliberately doesn't:

- Both naming shapes: `myFunc = #(define-…-function …)` and `#(define-public myFunc (define-…-function …))`. All four `define-…-function` forms count, since all four are called the same way.
- Signatures are aligned with the predicate list **from the right**, so the pre-2.15 `(parser location note)` argument lists still in use across real libraries read as one argument, not three.
- A predicate becomes an `ArgKind` only where its source *shape* is known (`ly:music?`, `ly:pitch?`, `string?`, the integer ones); everything else is `ArgKind::Unknown(predicate)`, consuming one node. So an unfamiliar predicate costs the extent of one argument, not the whole signature.
- Docstrings are converted from Texinfo to Markdown once, on the way in, `(_i "…")` wrappers included.
- Function *bodies* are not read at all, `#{ … #}` least of all: that is what keeps the two readers from becoming mutually recursive.

[`src/command/definition.rs`](../src/command/definition.rs) owns what a *file* defines: `Binding`, the `Definition` decorator that gives any command the place its file wrote the name, and the `Variable` a binding becomes when nothing says what arguments it takes. `command_assist::hover` returns `None` for a command with neither parameters nor prose, because otherwise every `\foo` reference to a plain variable pops up a box containing just `\foo`. Rendering a variable's *value* on hover is the obvious thing to do with that space, and is not done yet.

### Go-to-definition for user-defined music functions

A music function in a user's file is written as an ordinary assignment, `myFunc = #(define-music-function …)`, and `document.rs`'s symbol query captures a definition from the left-hand side alone:

```
(lilypond_program (assignment_lhs (symbol) @definition))
```

Since the signature comes from the right-hand side of the same assignment the query already sees, the two views of one definition have to agree; the tests in `tests/goto_and_references.rs` are what keeps them in step. A function defined only inside `#( … )` is not a second-class definition either: go-to-definition, find-references and rename all reach it.

## The install layer

The layer under the user's own files: what the active LilyPond installation binds. `\appoggiatura`, `\accent`, `\pp`, `\slurUp` and the several hundred others are defined in LilyPond's own initialisation files, in exactly the source the two existing readers already read — the install files *are* `.ly` files, `foo = #(define-music-function …)` and `foo = #(make-articulation 'foo)` alike. So this layer adds no new reading, only new files to point the readers at.

### Which files

LilyPond bootstraps itself by parsing `ly/declarations-init.ly` once per session (from `scm/lily/lily.scm`), and that file `\include`s the rest. So the definitions users can reach without including anything themselves are exactly what that closure binds, and the list is:

| File | What it contributes |
|---|---|
| `declarations-init.ly` | `\break`, `\noBreak`, `\fine`, direction words, the punctuation-named articulations (`"~"`, `"("`, `"\\<"`) |
| `music-functions-init.ly` | the bulk of it: ~112 `define-music-function`s, docstrings included |
| `toc-init.ly` | table-of-contents commands |
| `drumpitch-init.ly` | drum note names for `\drummode` |
| `chord-modifiers-init.ly` | chord-mode modifiers |
| `script-init.ly` | articulations and ornaments — `\accent`, `\fermata`, `\trill`, and the `-.`/`->` shorthand table |
| `chord-repetition-init.ly` | chord repetition (`q`) |
| `scale-definitions-init.ly` | the named scales |
| `dynamic-scripts-init.ly` | `\pp` … `\ffff`, `\sfz`, `\fp` |
| `spanners-init.ly` | spanner shorthands |
| `predefined-fretboards-init.ly`, `string-tunings-init.ly` | fretboard and tuning identifiers |
| `property-init.ly` | ~82 property shortcuts — `\slurUp`, `\stemDown`, `\accidentalStyle` |
| `grace-init.ly`, `midi-init.ly` | grace-note and MIDI setup identifiers |
| `paper-defaults-init.ly` | paper block defaults |
| `context-mods-init.ly` | `\with`-block helpers |

That is the closure in `\include` order, with one omission: **`engraver-init.ly`**, 50 KB of `\context { … }` blocks inside a `\layout`, whose bindings are engraver and context defaults rather than commands. The symbol query only captures *top-level* assignments, so parsing it for *commands* would yield almost nothing for its cost — that reasoning still stands, and this table stays as the list of files read for commands. But a `\context { \name … }` block is exactly where a context type is declared, so the file is read after all, for that: [`install::load`](../src/install.rs) has a second file list, `CONTEXT_FILES`, read by a second pass that feeds [`context::read`](../src/context.rs) instead of the command readers. `performer-init.ly` joins it there — LilyPond declares every context type twice, once as an engraver context (with its `\description`) and once as a performer context, and the two are merged. Neither file contributes anything to the table above; both are absent from it for the same reason as ever. See [`contexts.md`](contexts.md) for the context-type namespace this feeds.

Everything else under `ly/` is deliberately out too, because:

- `init.ly` is the driver LilyPond wraps around the user's own file (via `\maininput`), not a source of definitions.
- The note-name language files (`english.ly`, `deutsch.ly`, …) are one-line shims that do `\language "…"`; the pitch names themselves are a data table in `scm/lily/define-note-names.scm`, which [`note_names.rs`](../src/note_names.rs) reads directly — see [Note names](#note-names) below.
- `predefined-{guitar,mandolin,ukulele}-fretboards.ly` are 24–47 KB of generated chord-shape data yielding a handful of names.
- `articulate.ly`, `bagpipe.ly`, `gregorian.ly`, `satb.ly`, the `*-tkit.ly` templates and friends are optional features the user `\include`s explicitly — and when they do, the *workspace* layer reads them, as it does any other include. They belong to that layer, not this one.

Writing the closure out rather than following the `\include`s is the point: it lets the command list be trimmed (`engraver-init.ly` and `performer-init.ly`, read for context types alone via `CONTEXT_FILES` instead) and audited, and it keeps install loading out of the include-resolution machinery, which is built around a document graph these files are not in. The staleness that a fixed list risks is answered by a test: **every file in either list must exist in every install the tests find**, so a version that renames or drops one fails loudly instead of quietly losing commands or context types.

### Note names

The install layer carries one thing that is not a command: the note-name languages, read by [`NoteNames::read`](../src/note_names.rs) from `scm/lily/define-note-names.scm` (the spellings) and `scm/lily/lily-library.scm` (the alteration constants those spellings name, rationals in whole tones). They live on the layer beside its commands and context types, and `Scope::note_names` hands out the nearest set, so a pitch is resolved through the tables of the installation the document is actually being analysed against.

Everything that knows a language exists reads them from there: the note analyser starts in the default (Dutch, named in `note_names.rs` because the file doesn't say so) and follows whatever [`command::language`](../src/command/language.rs) returns, and `\language`'s completion offers every name the installation accepts, aliases included, described by that language's own seven naturals. `\language` and a language `\include` share the one `Command` impl, since LilyPond's `english.ly` and its siblings are one-line `\language "english"` shims.

A note-name language differs from an entry mode in *reach*, and `MusicContext` carries both: a `\chordmode`'s reading of its symbols stops at the closing brace, while a `\language` outlives it — LilyPond's parser switches note names for the rest of the parse. `Command::music_context` answers with both halves and the analyser applies each to its own extent.

With no readable installation there are no languages at all, and the analyser then says nothing about what is or is not a note rather than flagging every symbol in the score — the same "degrade quietly" rule the CamelCase context-name fallback follows.

### Finding the install

The client passes `lilypondShareDir` at `initialize`, pointing at `<share>/lilypond/<version>` — the directory whose children include `ly/`, `scm/lily/` and `vim/syntax/`. The words file, the `ly` directory and the note-name data are all found by joining onto it directly.

### How they're read

A new module, `src/install.rs`, owns the file list and the loading:

- For each file in order: read it, parse it, take the bindings the two readers between them produce — the same merge `Document` does, which wants extracting from `document.rs` as a `pub(crate)` function so `install.rs` can call it without building a whole `Document` (and without running note analysis over 88 KB of definitions for nothing).
- Fold every file's bindings into **one** `HashMap`, in file order, last binding winning — which is LilyPond's own resolution order, since it parses these files in the same sequence.
- Separately, `CONTEXT_FILES` is read the same way but through `context::read`, into a second `HashMap` of context types, merging `engraver-init.ly`'s and `performer-init.ly`'s two declarations of each name field by field (see "Which files" above and [`contexts.md`](contexts.md)). A context type's own name is also inserted into the command `HashMap`, as a zero-argument `Variable`, wherever a file-read command hasn't already claimed it.
- Build a single `Layer` from the command map, with the context-type map as its second namespace, and stack it into the workspace base (`vocabulary::workspace_base`).

One detail the merge must get right:

- **Install bindings drop their spans.** The `Definition` decorator carries a span with no file attached, which is fine for a layer that belongs to one document and wrong for one spanning fifteen files: go-to-definition would offer a range in whatever file the cursor is in. So `install.rs` builds its map directly — `binding.command` where there is one, `Variable` where there isn't — and `Command::definition()` stays `None`. Navigating from `\appoggiatura` into `music-functions-init.ly` is a natural follow-on, and needs a definition that carries a file as well as a span.

### When it happens

Synchronously, when the words file is loaded at `initialize`. An earlier sketch called for asynchronous indexing and an on-disk cache keyed on an install fingerprint; that is complexity to buy only once measurement asks for it, and measurement doesn't. If it ever does — a much larger install, a slower machine — the answers in order are: read fewer files; move it to a background task that swaps the layer in when ready (the fingerprint machinery already re-analyses documents when a layer changes, so nothing else has to know); and only then a disk cache.

Measured (via `examples/install_timing.rs`, release build): LilyPond 2.24.3 on Windows, `46ms` for `vocabulary::workspace_base` end to end, yielding an install layer of `607` entries; LilyPond 2.26.0, `50ms` and `645`. (`Layer::len()` now counts all three namespaces a layer can hold, so these are entries, not commands — of the 607, the extra over the previous count are context types, not new commands.) Comfortably synchronous.


## Later

- **Markup commands**, the ~173 `define-markup-command`s in `scm/lily/define-markup-commands.scm` plus a few smaller files. Worth having — they are what a user is typing when they are inside `\markup` — but they need a reading path this design doesn't have: a `.scm` file is a sequence of top-level Scheme forms with no LilyPond around them, so the LilyPond grammar can't be pointed at one as it stands. Wrapping the file in `#(begin … )` and teaching `scheme.rs` to descend through a `begin` is the cheap route. The definition form's own shape is friendly: `(define-markup-command (bold layout props arg) (markup?) …)` puts the name first in the argument list, and the right-alignment rule already in place drops `layout` and `props` for free.
- **Go-to-definition into the install**, which needs a definition that carries a file as well as a span. Blocks context types the same way it blocks commands: an install-layer `ContextType` carries a `name_span` but no file either, so `\new Staff` can't navigate into `engraver-init.ly` any more than `\appoggiatura` can navigate into `music-functions-init.ly`. See [`contexts.md`](contexts.md).
- **Hover rendering a variable's value**, which is what `command_assist::hover` currently declines to do.
- **Documentation from the published manual**, to put a section of the Notation Reference behind a command that has no docstring, or a fuller answer behind one that has. It can't come from the install, which ships no documentation at all, so it needs a fetch-and-cache of its own: [`manual-hover.md`](manual-hover.md).

### What the words file still knows that we don't

`lilypond-words` lists 870 command names for 2.24.3, and no context types — it strips bare (non-backslash) entries, which is where a context type would show up. So it compares fairly only against the install layer's *command* namespace, not its 607-entry total: that namespace holds 566 names (525 read from the files table above, plus one per context type for the bare-name reference `\Staff` needs, wherever a file-read command hasn't already claimed it), and 392 of the words file's 870 are missing from it — the same figure as before this change, since none of the words file's names are context types to begin with. Not a goal to close — a name with no signature costs nothing, and the words layer answers `is_known` perfectly well — but the day the words file becomes an irritation rather than a convenience, this is the bill, measured against 2.24.3:

| Group | Count | Why it's missing |
|---|---|---|
| `define-markup-command`s in `scm/` | ~162 | The deferred item above — `\bold`, `\hspace`, `\wordwrap`, `\with-color`, `\fret-diagram-terse` |
| Context and grob property names | 153 | `barNumberFormatter`, `clefGlyph`, `stringTunings`: arguments to `\set` and `\override`, not commands at all. The words file can't tell the two apart, and lists them with the same doubled backslash; we never wanted them |
| Definitions in `ly/` files outside the fixed list | 20 | `gregorian.ly`'s ancient notation (`virga`, `flexa`, `divisioMaior`) and its like: files a user `\include`s explicitly, at which point the *workspace* layer reads them. They belong to that layer |
| Reserved words | ~40 | `\repeat`, `\header`, `\book`, `\markup`, `\tempo`, `\unset`, `\version` — the hand-written keyword layer. `\new`, `\context`, `\change` and `\lyricsto` now have `builtin` entries; most of the rest never will |
| Paper and layout variables | ~10 | `mm`, `cm`, `pt`, `indent`, `unit`, the `toc*Markup` family — bound *inside* a `\layout` or `\paper` block, so not the top-level assignments `SYMBOL_QUERY` matches |
| Odds and ends | ~7 | `A`, `B`, `C`, harvested by LilyPond's own words generator out of a docstring's example. They used to be accepted as context references by the CamelCase rule; now that the rule is only a fallback, they are accepted because the words file itself lists them — `parse_words` strips the `\\` from `\\A` and binds `A` like any other word — which is the row this table is about. And `f`, written `"f" = #(make-dynamic-script "f")` because a bare `f` would collide with the pitch |

Two of those rows are the only ones that point at a gap in the *reader* rather than at a deliberate exclusion:

- **String-keyed assignments** are not read at all. `"f" = …`, and the punctuation articulations `"~"`, `"("` and `"\\<"` in `declarations-init.ly`, put a string where `SYMBOL_QUERY` wants a `symbol`. Only `\f` is a name a user types with a backslash, so the practical cost is one dynamic.
- **Bindings inside a `\layout` or `\paper` block** are invisible for the same reason the top-level-only query keeps reading `engraver-init.ly` for commands cheap even now that it's read for context types: reaching into those blocks here would mean reaching into them everywhere, and everywhere includes 50 KB of engraver defaults. `\mm` and friends are real commands users write, so this is a genuine miss, but not one to fix by loosening the query.

## Constraints to respect

- **Optional arguments are genuinely ambiguous** in general — LilyPond's own parser runs predicates on parsed values and backtracks — though in practice shape-matching plus the `ArgKind::Unknown`-declines-music rule (see "The risk" above) has so far covered every real signature this design reads, `\tweak`, `\shape` and `\footnote` included. `\override` remains outside this entirely: it takes no signature in any layer, being a keyword construct rather than a function. Where shape-matching genuinely can't express a signature, hand-write the impl rather than complicating `default_parse`.
- **The maintenance surface of the read layers is predicates, not functions** — about 30 predicates in real use against ~400 functions, and predicates change far more slowly. `ArgKind::Unknown` is what keeps one unrecognised predicate from voiding a whole signature.
- **`#{ … #}` makes the two readers mutually recursive.** Signatures alone dodge this. Anything that reads function *bodies* does not, so don't.

## Testing

- Every hand-written command gets unit tests in `src/command.rs` in the style of the existing ones, including a half-typed case that exercises stopping at a missing required argument.
- Tests that depend on what LilyPond ships run against every installation found, as `TESTING.md` describes, and name the version in the failure message.
