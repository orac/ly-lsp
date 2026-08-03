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
| `builtin` | Hand-written impls in this repo | **Build now** |
| `workspace` | Definitions parsed from the user's open and `\include`d files | Later |
| `install` | `define-music-function` and friends read out of the active LilyPond install | Later |

`workspace` outranks `install` because a user redefining `\foo` means theirs. `builtin` outranks both, because it exists precisely where the other two are absent or unhelpful.

**The keyword layer can only ever be hand-written.** `\repeat` is a reserved word in LilyPond's Bison grammar; the `repeat` in `ly-syntax-constructor.scm` is the constructor the parser calls, not a function reachable as `\repeat`. The same holds for `\context`, `\new`, `\override`, `\set`, `\with`, `\alternative`, `\change` and the mode-switching commands. No amount of Scheme reading will produce them. Conveniently this set is small, changes rarely between LilyPond versions, and is exactly the set whose documentation is worth tailoring by hand.

Conversely the ~400 music functions are not worth hand-writing, and hand-written entries can never cover user-defined functions at all. Those come from the later layers.

### Decisions already taken for the later layers

Recorded here because they shape the trait, not because they're being built yet.

- **Read, don't evaluate.** Recognising `(define-music-function (a b) (pred? pred?) "doc" …)` is a datum-shape match, not a computation. Reading avoids embedding a Scheme interpreter, and avoids executing workspace-authored code in the server process.
- **Use `tree-sitter-scheme` for the reading.** We already depend on `tree-sitter`, and it gives error recovery and incremental reparse — essential for workspace definitions being edited mid-keystroke, where a strict S-expression reader simply fails.
- **If evaluation ever becomes unavoidable, shell out to the user's own `lilypond`** rather than embedding an interpreter: run it once over a generated `.ly` that dumps every function's name, `ly:music-function-signature` and docstring, and cache the result keyed on the binary's path and mtime. This is the same data the manuals are generated from. It requires respecting VS Code's workspace trust, since it executes workspace-reachable code.
- **Match on the definition form, not on `define-public`.** Harvesting `define-public` fabricates commands that aren't callable as `\foo`. The reliable markers are `define-music-function` / `define-event-function` / `define-scheme-function` / `define-void-function` in value position, plus `foo = #(define-… )` in `ly/*.ly`.
- **Docstrings arrive as Texinfo** (`@var{}`, `@code{}`, wrapped in `_i` for gettext). Convert to Markdown once on the way in, not per hover.

## The design

### The trait

```rust
/// A `\word` the server understands: the arguments it takes, what to say about it on hover, and what to offer inside its arguments.
///
/// There is one impl per *source of knowledge*, not one per command: hand-written unit structs for the keyword layer, and later a single `SchemeCommand` struct instantiated once per definition read from the install or the workspace.
///
/// Deliberately object-safe: [`Vocabulary`] stores `Arc<dyn Command>` and hands them out by name, so no method may be generic or return `Self`. In particular `check` returns a `Vec` rather than `impl Iterator`, because an RPITIT would make the trait dyn-incompatible.
pub trait Command: Send + Sync {
    /// The name as written, without its leading backslash (`repeat`).
    fn name(&self) -> &str;

    /// The parameters this command expects, in source order.
    ///
    /// Impls that override [`parse_args`](Command::parse_args) still return their parameters here, because signature help and arity diagnostics read them even when the parsing is irregular. A command we know only by name — a `lilypond-words` entry with no definition behind it — returns an empty slice, and its following block is then read as music by the analyser's main loop, which is what happens today.
    fn signature(&self) -> &[Param];

    /// Consumes this command's arguments from the siblings following its keyword.
    ///
    /// The default walks [`signature`](Command::signature): a required parameter that fails to match stops consumption, so a half-typed `\repeat volta` yields the arguments seen so far rather than nothing; an optional parameter that fails is skipped and the next parameter tried against the same node.
    ///
    /// Override only when the shape can't be expressed as a parameter list at all — `\override`'s property path, `\tweak`'s backtracking. Do not override merely to reject a bad argument; that belongs in [`check`](Command::check), so that a wrong-but-parseable call still produces a structured [`CommandCall`] for the refactorings to work with.
    fn parse_args(&self, args: &mut ArgReader) -> Vec<Arg> {
        default_parse(self.signature(), args)
    }

    /// How the note analyser should read this call's music arguments — the mode `\relative` establishes, the chord region `\chordmode` establishes, the non-note region `\lyricmode` establishes.
    ///
    /// Takes the parsed call because the answer often depends on an argument: `\relative c'` reads its own reference pitch out of `call` and returns `MusicContext::Relative(pitch)`. Takes `ambient` because some contexts inherit rather than replace it. The default, [`MusicContext::Inherit`], is right for the overwhelming majority of commands.
    fn music_context(&self, _call: &CommandCall, ambient: MusicContext) -> MusicContext {
        ambient
    }

    /// Hover documentation, already rendered to Markdown. `None` for a command we recognise but can say nothing about.
    fn documentation(&self) -> Option<&Documentation> {
        None
    }

    /// The values worth completing at parameter `index`. Empty when the parameter is open-ended.
    fn completions(&self, _index: usize) -> &[Candidate] {
        &[]
    }

    /// Problems with a parsed call beyond "an argument didn't match" — `\repeat volta 0`, a repeat kind that isn't one of the four, a `\volta` outside any `\repeat`.
    ///
    /// Produces LSP [`Diagnostic`]s directly rather than going through the note pass's [`Problem`] enum. `Problem` is a closed, `Copy` enum of fixed variants, which suits the note reader's small fixed set of complaints but not this: command diagnostics are open-ended and command-specific, and a `SchemeCommand` built at runtime in steps 3–4 could not add variants to it at all. `ctx` supplies the span-to-range conversion that keeps this method from needing a `LineIndex` of its own.
    fn check(&self, _call: &CommandCall, _ctx: &CheckContext) -> Vec<Diagnostic> {
        Vec::new()
    }
}
```

Code actions are deliberately not a method here. `code_action` already owns an offer/resolve lifecycle (see [`src/code_action/README.md`](../src/code_action/README.md)); an action that wants command knowledge asks the table for it, keeping the dependency one-way.

### Supporting types

```rust
/// One parameter of a command's signature.
pub struct Param {
    /// The name from the definition (`weightList`), shown in signature help. `Cow` so hand-written impls can use literals while parsed ones own their strings.
    pub name: Cow<'static, str>,
    /// What this parameter looks like in source, and hence how to consume it.
    pub kind: ArgKind,
    /// Whether the parameter may be absent. LilyPond writes these as `(name default)` pairs and matches them by trying the predicate and backtracking; we approximate that by trying the shape.
    pub optional: bool,
}

/// The existing enum, extended with the argument forms the hand-written commands need and with the escape hatch that makes parsed signatures usable.
pub enum ArgKind {
    BareWord,       // \repeat volta
    Count,          // \repeat unfold 4
    NumberList,     // \volta 2,3
    Music,          // a block, or a single braceless note or chord
    Pitch,          // \relative c', \fixed c, \key c \major
    Word,           // the \major of \key c \major — an escaped_word argument
    String,         // \clef "bass", \language "english" — quoted or bare symbol
    PropertyPath,   // \set Staff.instrumentName
    /// A predicate we have no source-shape rule for, named so hover and signature help can still show it. Consumes exactly one node, which is right often enough to beat refusing the whole signature.
    Unknown(Cow<'static, str>),
}

/// How music inside a command's body is to be read. Mirrors the analyser's existing `Mode` and `Region` pair, which collapse into this once commands stop steering them by hand.
pub enum MusicContext {
    Inherit,
    Absolute,
    Relative(Pitch),
    Fixed(i8),
    Chord,
    /// Lyrics, drums, figures, markup, headers — scanned for nested music and directives, but bare symbols are not events.
    NonNote,
}

/// A cursor over the sibling nodes following a command keyword, shared by the default parser and by hand-written overrides so both consume arguments the same way.
pub struct ArgReader<'a> { /* children, src, next index */ }

impl ArgReader<'_> {
    /// Consumes one argument of `kind`, or returns `None` **without advancing**. The non-consuming failure is what lets an optional parameter be retried against the next parameter; overrides must preserve it.
    pub fn take(&mut self, kind: &ArgKind) -> Option<Arg>;

    /// The next node without consuming it, for overrides that need to look before they leap.
    pub fn peek(&self) -> Option<Node>;

    /// The index of the first unconsumed sibling — what `command::parse` returns to its caller.
    pub fn position(&self) -> usize;
}

/// What a command may consult while checking a call, and the means to report what it finds.
///
/// Checking is a whole-document pass, not part of the note analyser's walk: a command's complaint often depends on calls *other* than its own — `\volta` is only wrong because no `\repeat volta` encloses it — and those aren't all collected until the walk finishes. So `check` runs afterwards, over the completed [`Commands`], driven from [`Document::diagnostics`].
pub struct CheckContext<'a> {
    src: &'a str,
    lines: &'a LineIndex,
    /// Every call in the document, in source order, so a command can inspect the calls enclosing or nested within its own.
    calls: &'a Commands,
}

impl CheckContext<'_> {
    /// The source text a span covers.
    pub fn text(&self, span: Span) -> &str;

    /// The innermost call whose body contains `span` — what `\volta` asks to discover it has no `\repeat`.
    pub fn enclosing(&self, span: Span) -> Option<&CommandCall>;

    /// The calls nested directly inside `call`'s body — what `\repeat volta` asks to check its alternatives.
    pub fn nested(&self, call: &CommandCall) -> impl Iterator<Item = &CommandCall>;

    /// Builds a diagnostic at `span`, converting it to a range and filling in `source: "ly-lsp"` so every impl reports consistently. Prefer these over constructing `Diagnostic` literals.
    pub fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic;
    pub fn warning(&self, span: Span, message: impl Into<String>) -> Diagnostic;
}

/// Hover text plus where it came from, so the table can prefer our curated wording over LilyPond's when both exist.
pub struct Documentation {
    /// Markdown, ready for an LSP `MarkupContent`.
    pub markdown: String,
    pub source: DocSource,
}

pub enum DocSource { Curated, Workspace, Install }
```

### The table

The symbol table is [`Vocabulary`](../src/vocabulary.rs), which already answers "is `\foo` a command?" and now answers "…and what does it do?". Reusing it avoids a second registry and a second name; `is_known` becomes a thin wrapper over lookup plus the existing CamelCase context-reference rule.

```rust
pub struct Vocabulary {
    /// Hand-written impls, always present.
    builtin: HashMap<&'static str, Arc<dyn Command>>,
    /// Definitions from the user's files. Rebuilt when a defining document changes.
    workspace: HashMap<String, Arc<dyn Command>>,
    /// Read from the active LilyPond install.
    install: HashMap<String, Arc<dyn Command>>,
    /// Names from `lilypond-words` with nothing behind them. Known, but with an empty signature.
    known_names: HashSet<String>,
}

impl Vocabulary {
    /// The command `\name` refers to, resolved `builtin` → `workspace` → `install`, or a nameless placeholder for a `known_names` entry.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Command>>;

    /// Unchanged in meaning: whether `\name` is a command we recognise at all.
    pub fn is_known(&self, name: &str) -> bool;
}
```

Note that `command::Commands` is a different thing with a confusingly close name: it is the list of `CommandCall`s *found in one document*, not the table of commands that exist. Leave it be for now; renaming it to `CommandCalls` is a tidy-up worth doing separately.

## How the note analyser uses it

This is the part that makes step 1 worth doing on its own, and the part most likely to be got wrong.

`Analyser::handle_command` currently does two jobs at once: skipping over a command's arguments so they aren't misread as notes, and deciding what mode and region the command's body is read in. The first job is exactly `parse_args`. The second is exactly `music_context`. So `handle_command` collapses to:

1. Look the `escaped_word` up in the `Vocabulary`.
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

There is one shape that will *not* work, and it's worth pinning as a known limitation rather than discovering later: a function defined purely inside a Scheme block, `#(define-public myFunc (define-music-function …))`, produces no `assignment_lhs`, so it is invisible to the query. `\myFunc` then gets an **undefined-reference error** — a false positive, which is worse than a missing feature. Add a test recording today's behaviour and note it in `note_analyser.rs`'s limitations list; step 3's Scheme reader is what fixes it.

I haven't confirmed the grammar really produces `assignment_lhs` for an assignment whose value is a `#( … )` block — `examples/dump_tree.rs`, the tool for checking that, is currently missing from the crate (see below). Confirm it while writing these tests.

**Step 1 — the trait, hand-written commands, and every call site.** No Scheme, no new layers, no new LSP features.

1. Add the trait and supporting types to `src/command.rs`, with `default_parse` driving off `signature()`. Implement the new `ArgKind` variants in `ArgReader::take`.
2. Write `builtin` impls for the commands currently hard-coded in the two places: `\repeat`, `\volta`, `\alternative` (from `SPECS`), and `\relative`, `\fixed`, `\notemode`, `\chordmode`, `\drummode`, `\figuremode`, `\lyricmode`, `\addlyrics`, `\lyricsto`, `\markup`, `\markuplist`, `\header`, `\paper`, `\layout`, `\midi`, `\with`, `\clef`, `\set`, `\unset`, `\language`, `\include` (from `handle_command`), including their short-form aliases (`\chords`, `\lyrics`, `\notes`, `\drums`, `\figures`).
3. Delete `SPECS` and `spec_for`; point `command::parse` at the table.
4. Rewrite `handle_command` as the five steps above and delete the per-command `match`.
5. Extend `Vocabulary` with the layered structure, with only `builtin` and `known_names` populated.
6. Add `\key`, `\transpose`, and `\tempo`, whose arguments the analyser currently mis-reads, turning the step 0 tests green. Drop their entry from the "Known limitations" list in `note_analyser.rs`'s module docs.

Apart from item 6 this step is behaviour-preserving: the existing suites must pass unchanged.

**Step 2 — new LSP features off the table.** Signature help, argument completion, hover, semantic tokens for bare-word arguments, and `check`-based diagnostics. Each is independent; none needs the later layers.

**Step 3 — the workspace layer.** Parse `define-music-function` out of the user's own files with `tree-sitter-scheme`. This is where cross-file invalidation lands: a signature edit has to invalidate call sites through [`document_graph.rs`](../src/document_graph.rs), which makes command parsing a derived cross-file analysis rather than a per-document one. Expect this to be the largest architectural cost in the whole feature.

Two things fall out of this step. Signatures start coming from the right-hand side of an assignment the symbol query already sees, so the step 0 go-to-definition tests are the guard that the two views of the same definition stay consistent. And a function defined only inside `#( … )` becomes visible, which removes the false-positive undefined-reference diagnostic noted above.

**Step 4 — the install layer.** The same reader pointed at the active install's `.scm` and `ly/*.ly`, indexed asynchronously on startup and cached on disk keyed on an install fingerprint, so first diagnostics aren't blocked.

## Constraints to respect

- **Optional arguments are genuinely ambiguous.** LilyPond's parser runs predicates on parsed values and backtracks (`\tweak`, `\override`, `\shape`, `\footnote`). Shape-matching gets most of the way; where it can't, hand-write the impl rather than complicating `default_parse`.
- **The maintenance surface of the later layers is predicates, not functions** — about 30 predicates in real use against ~400 functions, and predicates change far more slowly. `ArgKind::Unknown` is what keeps one unrecognised predicate from voiding a whole signature.
- **`#{ … #}` makes the two readers mutually recursive.** Signatures alone dodge this. Anything that reads function *bodies* does not, so don't.

## Testing

- Every hand-written command gets unit tests in `src/command.rs` in the style of the existing ones, including a half-typed case that exercises stopping at a missing required argument.
- The existing `tests/extract/`, `tests/inline/` and `tests/explicit/` suites are the regression net for step 1, and must pass unchanged.
- The step 0 tests come off `#[ignore]` in step 1 and stay as the regression net for the two fixed mis-readings.
- Steps 3 and 4 must not make tests depend on an installed LilyPond. Check in fixture `.scm` and dump files from the start.
