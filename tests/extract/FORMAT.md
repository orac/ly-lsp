# Extract test format

## File layout

Each `.extract` file holds one or more cases separated by a line containing only `===`. Each case has two sections divided by `---`:

```
<annotated source>
---
<expected output or INVALID>
```

The annotated source is the LilyPond input with one `^` annotation line placed directly below the source line it selects from. The `^` characters indicate the selection: their column offset is the selection start and their count gives the length. Leading spaces align them with the source above.

```
foo = { c d e }
        ^^^^^
---
music = { c d e }

foo = { \music }
```

Multiple cases in one file share a common theme (usually the same or similar source):

```
foo = { c d e }
        ^^^^^
---
music = { c d e }

foo = { \music }
===
foo = { c d e }
        ^
---
music = { c }

foo = { \music d e }
```

## Expected output

The expected output is what the full document looks like after applying the extraction with the default variable name `music`. Two edits are applied in this order:

1. Replace the selection with `\music`.
2. Insert `music = <rhs>\n\n` at the start of the line that begins the enclosing top-level statement.

Because the insert position is always at or before the replacement, applying (1) first leaves the insert offset untouched.

**Common gotcha:** selecting a whole `{ }` block replaces the braces too, so the original site becomes `\music` (no surrounding braces), not `{ \music }`. Selecting bare events inside a block leaves the braces in place.

```
% Whole block selected — braces are gone at the call site
foo = { c d e }
      ^^^^^^^^^
---
music = { c d e }

foo = \music

% Bare events selected — braces stay
foo = { c d e }
        ^^^^^
---
music = { c d e }

foo = { \music }
```

For invalid selections the expected section is the single word `INVALID` with no other content.

## Writing a new test

1. Decide which file the case belongs to, or create a new `.extract` file in `tests/extract/`.
2. Write the source, then place a `^` line under the intended selection. Count columns carefully — the `^` range becomes the LSP `Range` passed to `music_extract_info`.
3. Mentally apply the two edits to compute the expected output:
   - What does the source look like after replacing the selected bytes with `\music`?
   - Where does `music = <rhs>\n\n` get inserted (start of the enclosing statement's line)?
4. Write the expected output below `---`, or write `INVALID` if the selection should be refused.
5. Run `cargo test extract_cases` to verify.

## Debugging a failure

Test labels are `<filename>[<index>]`, e.g. `basic.extract[0]`. The index is zero-based within the file.

**"expected INVALID"** — the extraction succeeded when it should have been refused. Check whether the new boundary check needs to be added to `music_extract_info`. Inspect the parse tree with a scratch test using `Document::new(src).tree.root_node()` or by printing node kinds from `descendant_for_byte_range`.

**"expected Some, got None"** — the extraction was refused when it should have worked. Common causes: `has_error()` returning true (check for unclosed brackets in the source), the container not being `expression_block`, or the `^` markers cutting across a node boundary.

**Output mismatch** — the left side is what `apply_extract` produced, the right is what the file says. To diagnose:

- Trace `replace_range`: which bytes are replaced? Does the column arithmetic in the `^` line match?
- Trace `insert_before`: which line does the insertion land on? For nested sources, remember the function walks up to the direct child of `lilypond_program` then back through siblings to find the assignment start.
- Recheck the expected output by simulating the two edits by hand on the source string.

## Multi-line selections

Use a `>` annotation below the start line and a `<` annotation below the end line:

- `>` line: leading spaces give the **start column** on the annotated source line.
- `<` line: leading spaces plus the count of `<` characters give the **end column** (exclusive) on the annotated source line.

```
foo = {
    a4 b c d
         >
    e f g
    <<<
    a' b
}
---
music = { c d
    e f }

foo = {
    a4 b \music g
    a' b
}
```

Here the selection starts at `c` (col 9 of the second line) and ends just after `f` (col 4 + 3 = 7 of the third line).

## Limitations

- The case separator `===` and section separator `---` must appear on their own lines; they cannot appear in source text or expected output.
