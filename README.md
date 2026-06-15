# LilyPond LSP

This project implements a language server for the LilyPond language. It allows an editor like VS Code to understand the structure of the LilyPond file, highlight errors, show documentation inline, and so on.

## Implementation

It's implemented in Rust, so use all the regular `cargo` commands to build, test, and run the server.

Parsing is handled by the [tree-sitter LilyPond grammar](https://github.com/nwhetsell/tree-sitter-lilypond), which gives us incremental reparsing and error recovery — both essential when parsing source that's mid-edit. The grammar ships as C, so **building requires a C compiler** (MSVC on Windows, or `cc`/`clang` elsewhere); `cargo` compiles it for you via the `cc` crate.

The refactorings and quick fixes (extract-to-variable and friends) follow a two-step offer/resolve pattern. See [`src/code_action/README.md`](src/code_action/README.md) for the trait, the lifecycle, and how to add a new action.

## Testing

Run all tests with `cargo test`.

The extract-to-variable refactoring has its own file-based test suite in `tests/extract/`. Each `.extract` file contains one or more cases: annotated LilyPond source (with a `^` underline marking the selection, like vscode-tmgrammar-test) followed by the expected document after the refactoring is applied. See [`tests/extract/FORMAT.md`](tests/extract/FORMAT.md) for the full format, how to write a new case, and how to diagnose a failure.

The inline-variable refactoring (the inverse) has a parallel suite in `tests/inline/`, where a `^here`/`^all` caret marks the cursor and the action to run; see [`tests/inline/FORMAT.md`](tests/inline/FORMAT.md). On top of that, `tests/extract.rs` drives every extract case through extract *then* inline and checks the resolved music is unchanged, exercising the two as inverses.

The make-explicit refactorings (make durations explicit, make pitches explicit, and both) share a third suite in `tests/explicit/`, where each case pairs one selection with the output of all three actions (`--- dur`/`--- pitch`/`--- both`); see [`tests/explicit/FORMAT.md`](tests/explicit/FORMAT.md).