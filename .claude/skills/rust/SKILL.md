---
name: rust
description: Use this skill to write or edit Rust code in this repo. It has coding style and structure advice.
---
Do not wrap lines based on width or number of characters. Wrap lines when it makes semantic sense, e.g. elements of a large `vec!` or struct initialization.

Use `cargo fmt` after every edit and `cargo clippy` to check your work on completing a change. No pre-existing clippy errors: even if you think an error is unrelated to your change, fix it now.

Liberal rustdoc: show how the documented function or struct fits into the overall system, and requirements that aren't expressed in the type system (e.g. passing -1 for an integer argument has a special meaning).