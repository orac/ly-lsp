# LilyPond LSP

This project implements a language server for the LilyPond language. It allows an editor like VS Code to understand the structure of the LilyPond file, highlight errors, show documentation inline, and so on.

## Implementation

It's implemented in Rust, so use all the regular `cargo` commands to build, test, and run the server.

Parsing is handled by the [tree-sitter LilyPond grammar](https://github.com/nwhetsell/tree-sitter-lilypond), which gives us incremental reparsing and error recovery — both essential when parsing source that's mid-edit. The grammar ships as C, so **building requires a C compiler** (MSVC on Windows, or `cc`/`clang` elsewhere); `cargo` compiles it for you via the `cc` crate.

## Testing

Run all tests with `cargo test`.

The extract-to-variable refactoring has its own file-based test suite in `tests/extract/`. Each `.extract` file contains one or more cases: annotated LilyPond source (with a `^` underline marking the selection, like vscode-tmgrammar-test) followed by the expected document after the refactoring is applied. See [`tests/extract/FORMAT.md`](tests/extract/FORMAT.md) for the full format, how to write a new case, and how to diagnose a failure.