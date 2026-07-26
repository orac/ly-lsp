# Incremental note analysis

This is a proposal for a change that could be made to note analysis to avoid doing a full reparse every edit. It doesn't describe live code (yet).

> Starting this cold? Read [Implementation handoff](#implementation-handoff) at
> the end first — it maps the design onto the actual files, invariants and
> wiring points, and lists the gotchas worth knowing before the first edit.

## The problem

`note_analyser::analyse` walks the whole parse tree and rebuilds the entire
`NoteAnalysis` — every `Event`, `Problem` and `CommandCall` — from scratch. The
`Document` runs it again on every `didChange`. Tree-sitter, by contrast,
reparses incrementally: it reuses every subtree outside the edited region and
only does real work near the change. We'd like the note analyser to do the same
— reuse the analysis on either side of an edit and recompute only a bounded
region around it.

This document describes how that algorithm works. It does *not* change any
observable output: the incremental result must be byte-for-byte equal to the
from-scratch result. That equality is both the correctness criterion and the
main test (see [Testing](#testing)).

## Why this is harder than tree-sitter's reparse

Tree-sitter can localise its work because a node's shape depends only on the
tokens it spans. Our analysis has no such locality. It is a **left-to-right fold
with a carried state**, and several pieces of that state reach arbitrarily far
forward:

- the `\relative` reference pitch — change one note's pitch and *every*
  following note in the block gets a different resolved octave;
- the inherited duration (`last_duration`) — `c4 d e` gives `d` and `e` a `4`;
  edit the `4` and the tail re-inherits;
- the active note-name `language` — a `\language` directive re-spells, and can
  re-validate or invalidate, every note after it;
- `last_pitch` / `last_chord`, which feed bare-duration and `q` repetition.

So "re-analyse only the bytes tree-sitter marked as changed" would produce
*wrong* events. The unit of reuse cannot be a byte range; it has to be a point
at which the **carried state is known**, so that everything after it is a pure
function of the state and the (shifted) tail of the tree.

The shape of the solution is the one used by incremental lexers: resume the fold
from a saved state just before the edit, replay forwards through the change, and
stop as soon as the carried state re-converges to what it was — after which the
old output, shifted by the edit's length delta, is still valid. The difference
from tree-sitter is that our stopping point is governed by *state
re-equalisation*, not just by where the syntax stopped changing.

## The carried state

Model the analyser as a fold. To checkpoint and resume it we must name its state
exactly. Today it is split between the `Analyser` struct (global, threads across
the whole document) and the parameters/locals of `walk` (per open block):

**Global state**, shared across all blocks:

- `language: Language`
- `last_duration: Duration`
- `last_pitch: Option<Pitch>`
- `last_chord: Vec<ChordNote>`

**Per-block frame**, one for each block currently open from the root down to the
cursor (the recursion stack of `walk`):

- the block node (so we can fetch its children),
- `i`: the next child index to read,
- `mode: Mode` — crucially this carries `Relative(pitch)`, which *evolves within
  the block* as notes are read,
- `region: Region`,
- the loop locals `after_event: bool` and `pending: Option<Region>`.

A complete **checkpoint** is therefore `(global state, stack of frames)`. Given a
checkpoint and the tree, the rest of the analysis is fully determined.

### Making the state first-class: the work-stack refactor

`walk` is today a native recursion, so the "stack of frames" lives implicitly on
the Rust call stack and can't be cloned, saved, or resumed from the middle. The
enabling structural change is to **drive the walk with an explicit stack**:

```rust
struct Frame<'tree> {
    block: Node<'tree>,
    children: Vec<Node<'tree>>, // or fetch lazily
    i: usize,
    mode: Mode,
    region: Region,
    after_event: bool,
    pending: Option<Region>,
}

struct Walker<'a> {
    // the former Analyser globals
    language: Language,
    last_duration: Duration,
    last_pitch: Option<Pitch>,
    last_chord: Vec<ChordNote>,
    // the explicit recursion
    stack: Vec<Frame<'a>>,
    // outputs, as today
    events: Vec<Event>,
    problems: Vec<Problem>,
    commands: Vec<CommandCall>,
}
```

The driver pops the top frame, reads `children[i]`, and either emits an event,
pushes a child block as a new frame, or advances `i`. Entering a block pushes;
running off the end of a frame's children pops. This is a mechanical
transformation of the current `match` in `walk`, with the `mode`/`region`
parameters becoming frame fields and the `i`/`after_event`/`pending` locals
likewise.

With this in place a checkpoint is just `(globals, stack.clone())`, and resuming
is "install this state and run the driver". That is the entire reason for the
refactor; it should land first, on its own, with **no behavioural change**
(Phase 0 below).

## Checkpoints

Record a checkpoint at each **block boundary**: on entering a block (`{` or
`<<`, just after the frame is pushed and before its first child is read) and on
leaving one (`}` or `>>`, just after the frame is popped). Not only top-level
blocks — every block at every depth. A checkpoint is:

```rust
struct Checkpoint {
    pos: usize,        // byte offset of the boundary (an old-tree offset)
    state: SavedState, // globals + frame stack, minus the tree Nodes
}
```

The frames in a saved checkpoint can't hold `Node`s (they belong to a tree that
the next edit invalidates). Store instead each frame's **block start offset**;
on resume we re-fetch the live node from the *new* tree by that offset
(`descendant_for_byte_range`), which is valid for any block whose start is in the
unchanged prefix. So `SavedState` keeps `mode`, `region`, `i`, `after_event`,
`pending`, and a `block_start: usize` per frame, plus the four globals.

Checkpoints form a sorted log `C = [(p₀, s₀), (p₁, s₁), …]`, keyed by `pos`,
carried on the `NoteAnalysis` itself.

**Why block boundaries.** They are a sparse, naturally-occurring set of points
that line up with how LilyPond documents are built and edited. The common
structure is music in variables — `melody = \relative c' { … }`, `bass = … { … }`
— assembled by a `\score { … }` at the end. Block boundaries are also where a
writer tends to re-establish state explicitly: a phrase often opens with a note
that spells its own octave and duration, so the carried state entering the block
is "clean" and an edit's effect is more likely to die at the closing brace. An
edit inside one variable then resumes at that variable's `{`, replays only that
block, and splices every later variable and the score unchanged.

The price of sparseness is that resume and splice points snap to block edges: an
edit deep in a long flat block replays from the block's `{`, and convergence
can only be detected (and the tail spliced) at the next block boundary, even if
the fold re-converged an event or two earlier. For the variable-per-phrase
structure above that is exactly the granularity we want. A single monolithic
block with hundreds of notes degrades towards re-folding the whole block — if
that ever shows up in a profile, the remedy is to add intra-block checkpoints
(every *N* events), which the design accommodates without change.

**Memory.** The cloned frame stack is `O(depth)` per checkpoint and depth is
small; with checkpoints only at block boundaries the log is `O(blocks)`, far
smaller than the event count. `last_chord` is cloned into each, which is cheap at
this density. If it ever matters, share immutable ancestor frames behind `Rc`.

## The dirty frontier and the quiescence frontier

Both ends of the work come from tree-sitter's own diff. After the edit and
reparse, take `old_tree.changed_ranges(&new_tree)` (computed *before* the old
tree is discarded). Let the changed ranges be the region tree-sitter had to
restructure — this can extend a little *before* the literal edit (backward
re-lexing: a `'` joining a preceding symbol, editing inside a number) and a
little *after* it.

- **Dirty frontier** `D` = the minimum start over the changed ranges, also
  clamped by the edit's own `start_byte`. Nothing before `D` differs in either
  text or tree, so any checkpoint at `pos ≤ D` is reproducible verbatim.
- **Quiescence frontier** `Q` (in old-tree coordinates) = the maximum end over
  the changed ranges. Beyond `Q` the new tree is the old tree shifted by `delta`
  (the edit's length change, possibly negative). This is where the tail becomes
  reusable *if* the carried state has also re-converged.

Using `changed_ranges` for both ends — rather than the raw edit range — is what
makes the splice safe against tree-sitter's backward and forward re-lexing.

## The algorithm

Given the previous `NoteAnalysis`, its checkpoint log `C`, the edit (`delta`),
and the old and new trees:

1. **Find the resume point.** Binary-search `C` for the last checkpoint
   `(p_k, s_k)` with `p_k ≤ D`. If there is none (the edit is before the first
   checkpoint), fall back to a full from-scratch analysis — always correct, just
   not incremental.

2. **Reuse the prefix.** Keep every `Event`, `Problem` and `CommandCall` whose
   span ends at or before `p_k`. Their offsets are unchanged (they lie below
   `D ≥ p_k`). Keep the checkpoints up to `p_k` too.

3. **Resume.** Rebuild the live frame stack of `s_k` against the *new* tree
   (each frame's `block_start` still resolves, being `≤ D`), restore the globals,
   and set the innermost frame to continue just after the event at `p_k`.

4. **Replay.** Run the driver forward over the new tree, emitting fresh events,
   problems and commands, and recording fresh checkpoints, exactly as a
   from-scratch pass would from this state.

5. **Detect convergence.** At each block boundary whose new position `q` lies at
   or beyond the (shifted) quiescence frontier `Q + delta`, look up the old
   checkpoint at the corresponding old position `q − delta`. If one exists and
   its `state` equals the current carried state, the fold has re-converged:
   stop. (Block boundaries are the only positions with a saved old state to
   compare against — which is also where the comparison is most likely to
   succeed, per the heuristic above.)

6. **Splice the tail.** Append the old events/problems/commands with span start
   `≥ q − delta`, each shifted by `+delta`, and the old checkpoints likewise. By
   construction the last replayed event ends at `≤ q` and the first spliced one
   starts at `≥ q`, so spans stay disjoint and ordered.

   If convergence never happens (e.g. a `\language` change near the top, or an
   unterminated brace that reshapes everything after it), replay simply runs to
   end of file. That is the worst case: correct, with no saving on the tail.

7. **Suppress diagnostics.** Re-run the error-span suppression as a whole-
   document final pass over the merged problem list (see below).

`delta = new_text.len() − (old_end_byte − start_byte)` and may be negative for a
deletion; every shift is a signed offset add.

### Why convergence, not just `changed_ranges`, bounds the replay

Beyond `Q` the *tree* is identical-shifted, but our *output* there is only valid
if the carried state matches what produced it the first time. A pitch edit at the
top of a `\relative` block changes the reference for the whole block even though
tree-sitter reports only the one note as changed; the replay must continue,
re-resolving octaves, until the running reference happens to coincide with the
old one again (often immediately after the edited note, sometimes never). The
state-equality check at step 5 is precisely the test for "the tail is still
correct", and it is the one place this design genuinely differs from tree-sitter's
structural reuse.

## Shifting reused output

The spliced tail items move by `delta`. This needs a uniform signed shift over
every byte-valued field:

- `Event`: `span`, `value_end`.
- `Problem`: its `Span`.
- `CommandCall`: `keyword`, `span`, and each `Arg`'s span(s).
- `Checkpoint`: `pos`, and each frame's `block_start`.

`RelativeRef.text` and all resolved `Pitch`/`Duration` values are offset-free and
carry over unchanged — which is the whole point of stopping at state convergence.
A small `fn shift(&mut self, delta: isize)` (or `shifted(self, delta)`) on each
type keeps this honest.

## Diagnostics suppression

`analyse` currently drops any `Problem` that falls inside an `ERROR` span, so a
mid-edit file isn't buried in spurious squiggles. To keep the incremental core
simple, treat suppression as a **view over an unsuppressed problem list**:

- the incremental state stores problems *before* suppression;
- after each rebuild, collect the new tree's error spans and `retain` over the
  merged list.

`collect_error_spans` is already lazy — it prunes any subtree with
`!has_error()`, so on a mostly-clean tree it returns almost immediately and only
descends into genuinely broken regions (which cluster around the edit anyway).
That makes a full re-suppression pass cheap enough to leave non-incremental for
now. Making it incremental (only subtrees overlapping the changed ranges can gain
or lose errors) is a later optimisation if it ever shows up in a profile.

## Wiring into `Document`

`Document::apply_change` already performs the tree-sitter `edit` + reparse
(`document.rs:100`–`109`). Two changes:

- capture `let changed = self.tree.changed_ranges(&new_tree)` *before* replacing
  the old tree, and the `delta` from the splice it already computes;
- replace the unconditional `note_analyser::analyse(&tree, &text)` in
  `from_parts` with the incremental entry point, threading through the previous
  `NoteAnalysis` and checkpoint log.

The checkpoint log is a field on `NoteAnalysis` (it is produced by, and consumed
by, the note pass, so it belongs with the rest of that pass's output). The
symbol/include/diagnostic extraction (`extract`) is a separate full pass and is
**out of scope** here; the same technique could be applied to it later, but
YAGNI.

Sketch of the entry point:

```rust
/// Incrementally rebuild the analysis after an edit, falling back to a full
/// pass when no usable resume checkpoint exists.
pub fn reanalyse(
    previous: &NoteAnalysis,
    checkpoints: &[Checkpoint],
    new_tree: &Tree,
    src: &str,
    changed: &[tree_sitter::Range],
    delta: isize,
) -> (NoteAnalysis, Vec<Checkpoint>);
```

## Testing

The decisive test is **differential**: for a corpus of snippets and a sequence of
random edits, assert that `reanalyse(...)` equals a fresh `analyse(...)` of the
same text — full `NoteAnalysis` equality (events, problems, commands) — **at
every error-free state**. Because deferral deliberately serves a stale tail while
the tree holds an `ERROR`, the comparison is only meaningful once the tree is
quiescent again; the loop should edit freely but assert equality only where the
new tree has no error (and, as a stronger check, after each broken run resolves,
confirm the reconciled result matches from-scratch). A fixed-seed fuzz loop (or
`proptest`) over insert/delete/replace edits — including sequences that open a
brace, type inside it, then close it — exercises the resume/replay/splice and
defer/reconcile paths far more thoroughly than hand-written cases. The existing suites (`note_analyser` unit tests,
`tests/extract`, `tests/inline`, `tests/explicit`) all keep passing unchanged,
since they assert against from-scratch behaviour the incremental path must match.

Targeted cases worth writing by hand, each chosen to hit one path:

- a local edit that converges immediately (add an articulation; change a pitch in
  *absolute* mode);
- an edit inside `\relative` that shifts the reference and so replays further;
- a `\language` change near the top, forcing replay to EOF (no convergence);
- a deletion (negative `delta`) and an insertion at a block boundary;
- a batched `didChange` carrying several content changes at once.

## Phased delivery

Phases 0–2 are all in scope; the split is a build order, not a set of optional
stopping points. Phase 1 on its own (prefix reuse only) would help append-style
edits, but those are *not* the common case here: the music lives in variables
near the top of the file and the `\score` block sits at the end, so a typical
edit has a large, reusable *tail*, not a large reusable prefix. The tail-splice
in Phase 2 is what makes that structure cheap, so it is part of the committed
work.

1. **Phase 0 — explicit work-stack.** Turn `walk`'s recursion into the `Walker`
   driver with no behavioural change. Land green against all existing tests.
2. **Phase 1 — checkpoints + prefix reuse.** Record checkpoints; add `reanalyse`
   doing resume + replay-*to-EOF* (no early stop yet) + prefix reuse. Simple to
   get right, and the rung on which the differential test is stood up.
3. **Phase 2 — convergence, tail splice, and `ERROR` deferral.** Add the
   quiescence check and the shifted-tail splice — where the common "edit one
   variable, reuse every later variable and the score" saving lands — together
   with the defer-while-broken handling and the accumulated `deferred_dirty`
   span, so structure-breaking keystrokes hold the old tail instead of replaying
   garbage to EOF.
4. **Phase 3 — tuning (only if profiling asks).** Intra-block checkpoints for
   monolithic blocks, `Rc`-shared ancestor frames, incremental error-span
   collection, incremental `extract`.

## Alternatives considered

- **Re-analyse only the changed ranges.** Rejected: ignores the long-range
  carried state, so it produces incorrect octaves, durations and spellings. The
  carried state is the whole difficulty.
- **Per-top-level-block memoisation.** Re-fold a whole top-level block when its
  entry-state or subtree changed, reuse the rest. Simpler — no mid-block resume,
  no work-stack refactor — and a reasonable stepping stone. Weakness: a one-note
  edit in a single large movement (one big `{ … }`) re-folds the entire movement,
  which is the common no-win case. The checkpoint design degrades to this only
  when checkpoints are coarse, and does better when they're dense, so it
  subsumes this alternative rather than competing with it.

## `ERROR` nodes: defer rather than replay

While typing, the tree frequently holds an `ERROR` node — an unclosed brace, a
half-written command. An `ERROR` can make `changed_ranges` report a region
reaching to end of file, pushing the quiescence frontier `Q` to EOF and stopping
the tail-splice from firing until the syntax is briefly valid again. Replaying
to EOF on every such keystroke is always *correct*, but it both wastes work and
blunts Phase 2's saving during exactly the keystrokes we most want it.

The transient tail produced in that state is, in any case, garbage we are about
to discard: an unmatched `{` swallows the rest of the file into the unclosed
block, and we already suppress diagnostics under `ERROR` spans, so the user is
not shown it. So rather than recompute it, **defer**: while an edit leaves the
tree broken, keep the previous analysis for everything past the break, shift its
offsets by the edit's `delta`, and mark the result provisional. Do no replay.
When a later reparse returns a clean (or at least un-broken-in-that-region) tree,
run one normal incremental `reanalyse` to reconcile, then clear the provisional
state. Transient errors localise — they come from typing, and once the edit is
finished they resolve in one place — so the usual path is: several deferred
keystrokes, then a single reconciling pass that splices the long-unchanged tail.

**Accumulating the dirty region.** Reconciling incrementally (rather than from
scratch) on clear needs the union of the changed ranges seen since the last clean
state, kept offset-correct across each intervening edit — a single growing
`deferred_dirty: Option<Span>`. On clear, its start is the dirty frontier `D` for
the reconciling pass, so we still resume from a checkpoint before the earliest
deferred edit and reuse the prefix. A single span suffices for the common "errors
localise to one place" case; two disjoint trouble spots (a stale broken region
far away *while* editing a good region) make the union span both and collapse the
reconcile to replay-to-EOF — correct, just unsaved. Multi-region tracking is a
later refinement, taken only if that pattern shows up in practice.

**Effect on the invariant.** Deferral deliberately lets the analysis differ from
a from-scratch pass *while the tree is broken* — the provisional tail is stale,
not fresh-garbage. The correctness criterion therefore weakens to: the
incremental result equals the from-scratch result **whenever the tree is
quiescent (error-free)**. That is the honest statement — mid-keystroke output
over a syntactically incomplete document is not a ground truth worth matching —
and it is what the differential test should assert: drive a sequence of edits,
but compare against from-scratch only at the error-free states.

This subsumes the earlier worry about `Q` jumping to EOF: we no longer try to
splice across a live error at all, we wait it out. It is still worth *measuring*
how often deferral engages and how long it persists, to confirm the "localises
and resolves quickly" assumption holds on a real editing session.

## Settled decisions

- **Checkpoint density:** sparse, at block boundaries (`{`/`}`/`<<`/`>>`) at all
  depths — not per event, not only top-level. Intra-block checkpoints stay a
  Phase 3 fallback for monolithic blocks.
- **Where the log lives:** a field on `NoteAnalysis`.

## Implementation handoff

Concrete anchors for picking this up with no prior context. Line numbers are
approximate — search by the names.

### Read these, in order

- The **module doc at the top of `src/note_analyser.rs`** — what counts as a
  note, the modes, and the *deliberate* known limitations. Several are
  approximate by design; don't "fix" them while refactoring.
- `analyse`, the `Analyser` struct, and `walk` in the same file — the fold to be
  made resumable. Note the state split: globals on `Analyser`
  (`language`, `last_duration`, `last_pitch`, `last_chord`); per-block
  `mode`/`region` as `walk` parameters; `i`/`after_event`/`pending` as loop
  locals. Phase 0 lifts all of this into explicit `Frame`s.
- `src/notes.rs` — `NoteAnalysis` (where the checkpoint log will live), `Event`
  and `Events::new` (the ordering invariant), `Problem`, `RelativeRef`, `Span`.
- `src/document.rs` — `apply_change` (~81–111), `from_parts` (~63), `parse`
  (~390), `point_at` (~464). All the wiring is here.
- `src/command.rs` — `CommandCall`, `command::parse`. Commands are recorded in
  the same pass, in preorder.
- `examples/dump_notes.rs` to eyeball the analyser on a file; the
  `grammar-inspector` agent to ask how a snippet parses (node kinds, tree shape)
  instead of writing a throwaway dump.

### Invariants to preserve (the tests guard them)

- `Events::new` debug-asserts events are in source order with non-overlapping
  spans. The prefix + replayed-region + shifted-suffix splice must keep that
  true — convergence is detected at a block boundary that is an event boundary in
  both trees, so the join is clean by construction; don't break it with an
  off-by-one on the splice index.
- Commands are preorder (outer `\repeat` before its nested `\volta`); see
  `Commands::new` and the `commands_are_recorded_in_source_order` test.
- Diagnostic suppression today is a **post-pass** that drops `Problem`s under
  `ERROR` spans (`analyse`, just after the walk; `collect_error_spans`,
  `under_error`). The design keeps problems *unsuppressed* in the incremental
  state and applies suppression as a final view over the merged list.
  `collect_error_spans` is already lazy (it prunes any subtree with no error), so
  a whole-document suppression pass stays cheap.
- Offsets are **bytes** (UTF-8), half-open `[start, end)`. LSP positions are not
  offsets; the conversion already happens in `apply_change` via `LineIndex` and
  `point_at`. Stay in byte space throughout the analyser.

### Wiring specifics

- `document_graph.rs::change` (~64) applies the content changes of one
  `didChange` **one at a time**, each its own `apply_change`. So every
  `apply_change` is a single incremental step, and a multi-change notification is
  a *run* of steps — each of which may leave an `ERROR`, which is exactly the
  deferral path. No need to coalesce changes.
- `apply_change` already computes `start_byte`/`old_end_byte`/`new_end_byte` and
  edits then reparses the tree (~100–109), then calls `from_parts`, which runs
  `analyse` from scratch (~110). To go incremental: **before** the old (edited)
  tree is dropped, capture `let changed = self.tree.changed_ranges(&new_tree);`
  compute `delta = new_text.len() as isize - (old_end_byte - start_byte) as
  isize;` and hand the previous `NoteAnalysis` (carrying its checkpoint log),
  `changed`, and `delta` to the new `reanalyse`. The old tree is currently
  discarded when `*self` is reassigned — keep it alive until after
  `changed_ranges`.
- Keep a from-scratch path: `Document::new` still needs it, and `reanalyse`
  falls back to it whenever there is no usable resume checkpoint.
- `extract` (symbols/includes/diagnostics, `document.rs:405`) is a **separate**
  full pass and is out of scope.

### Type and lifetime gotchas

- `Mode` (`note_analyser.rs`) currently derives only `Debug, Clone, Copy`. Add
  `PartialEq, Eq` for the convergence equality check. `Region` already has them;
  `Pitch`/`Duration`/`ChordNote`/`Language` are already `Eq`.
- A live `Frame` holds `Node<'tree>` (it borrows the tree). A **checkpoint must
  not** — store each frame's `block_start: usize` and re-fetch the live node from
  the new tree on resume (`descendant_for_byte_range(start, start)`). Verify that
  returns the block node and not an inner token; walk up to the expected kind if
  not.
- The loop locals `after_event` and `pending` are genuine carried state — easy to
  forget when lifting the recursion to an explicit stack, and their loss is the
  kind of bug the differential test will catch only intermittently. Capture them
  in the frame.

### Order of work and a sanity check first

1. **Measure before optimising.** Confirm full re-analysis is actually a cost
   worth removing — on realistic files it may already be fast enough that this is
   chiefly an architecture/learning exercise. Time `analyse` on a large score (or
   add a `criterion` bench) before committing to the whole thing.
2. **Phase 0** — lift `walk` to an explicit `Walker`/`Frame` stack, *no
   behaviour change*, `cargo test` green, landed on its own.
3. Stand up the **differential harness** (random edits; compare `reanalyse` to a
   fresh `analyse` *at error-free states only* — see [Testing](#testing)).
4. **Phase 1** then **Phase 2** as described under
   [Phased delivery](#phased-delivery).

Test gate: `cargo test` (unit tests at the foot of `note_analyser.rs`, plus the
file suites in `tests/extract`, `tests/inline`, `tests/explicit`). The fiddliest
code to test deliberately — not just via the fuzzer — is the multi-edit broken
run followed by a resolve, where `deferred_dirty` and the shifted-but-unreplayed
tail accumulate across keystrokes.
