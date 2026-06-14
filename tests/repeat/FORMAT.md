# Unfold-repeat test format

For the [`inline_repeats`](../../src/code_action/inline_repeats.rs) refactoring
("Unfold repeat"). Each `.repeat` file holds one or more cases separated by a
line containing only `===`. Each case has two sections divided by `---`:

```
<annotated source>
---
<expected output or NONE>
```

## The annotation

A single caret line placed directly below the source line it points into marks
the **cursor position**:

```
\repeat unfold 2 { c4 d }
  ^
---
{ c4 d c4 d }
```

The caret's column is the cursor offset. It must fall within the
`\repeat <kind> <count>` header for the action to be offered.

## Expected output

The full document after the action's edits are applied. Unfolding makes a note's
duration or octave explicit only where it would otherwise resolve differently
once the repeat is expanded, so the result often gains an explicit `4` or octave
mark that the folded form left implicit — that is intended, and matches what
LilyPond's `\unfoldRepeats` produces.

For a cursor where the action should not be offered (a `tremolo` repeat, or a
cursor outside any repeat header), the expected section is the single word
`NONE`.
