# Make-explicit test format

For the three "make explicit" refactorings (make durations explicit, make
pitches explicit, and make both explicit). Each case pairs one input with the
output each of the three actions produces, so every input is exercised by all
three. Cases are grouped into topical `.explicit` files (basics, relative, ties,
accidentals, …); within a file they are separated by a line containing only
`===`:

```
<annotated source>
--- dur
<expected output or NONE>
--- pitch
<expected output or NONE>
--- both
<expected output or NONE>
```

## The annotation

The selection is marked below its source line(s), exactly as in the
[extract format](../extract/FORMAT.md):

- single line: a run of `^` under the selected columns.
- multi line: `>` under the start line and `<` under the end line.

The leading spaces give the start column; the marker run gives the end column.
Unlike the earlier format the marker carries no action word — the same selection
is run through all three actions.

## The expected sections

Each `--- <action>` header introduces the full document after that action's
edits are applied to the selection. All three sections must be present, in any
order. The action words are:

- `dur` (or `durations`) — **Make durations explicit**.
- `pitch` (or `pitches`) — **Make pitches explicit**.
- `both` — **Make pitches and durations explicit**.

The actions only *add* the inherited pitch or duration where the source omits
it; an event that already writes one is left untouched. Where the chosen action
should not be offered — nothing in the selection omits the relevant attribute —
the section is the single word `NONE`. `offer` and `resolve` share the
availability check, so an unoffered case also resolves to no edit.

```
{ c4 4 e }
  ^^^^^^^
--- dur
{ c4 4 e4 }
--- pitch
{ c4 c4 e }
--- both
{ c4 c4 e4 }
```

Here the bare `4` only ever repeats its pitch (it already writes its duration),
while `e` only ever gains its inherited duration, so `dur` touches just `e`,
`pitch` touches just the bare `4`, and `both` touches both.
