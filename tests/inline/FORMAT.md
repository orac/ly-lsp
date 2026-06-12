# Inline test format

The inverse of the [extract format](../extract/FORMAT.md), for the
inline-variable refactorings. Each `.inline` file holds one or more cases
separated by a line containing only `===`. Each case has two sections divided by
`---`:

```
<annotated source>
---
<expected output or NONE>
```

## The annotation

A single caret line placed directly below the source line it points into marks
the **cursor position** and which action to run:

- `^here` runs **Inline here** (inline this one reference, keep the definition).
- `^all` runs **Inline all** (inline every reference, delete the definition).

The caret's column is the cursor offset; the word after it picks the action.
`^all` may sit on a `\foo` reference or on the `foo = ` left-hand side.

```
music = { c4 d e }

foo = { \music f }
          ^here
---
music = { c4 d e }

foo = { c4 d e f }
```

## Expected output

The full document after the chosen action's edits are applied. Because inlining
only *adds* the durations and octaves needed to preserve meaning (it never
strips ones that have become redundant), the result often differs from the
pre-extract original by an explicit `4` or octave mark — that is intended.

For a cursor where the action should not be offered, the expected section is the
single word `NONE`.

## Round-trip coverage

`tests/extract.rs` additionally drives every `.extract` case through extract
*then* inline and checks the resolved music is unchanged (same pitches and
durations), so the two refactorings are exercised as inverses without needing
the text to match byte-for-byte.
