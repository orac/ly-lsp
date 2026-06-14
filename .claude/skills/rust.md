---
name: Rust
description: Use this skill to write or edit Rust code in this repo. It has coding style and structure advice.
invokable: false
---
Do not wrap lines based on width or number of characters. Wrap lines when it makes semantic sense, e.g. elements of a large `vec!` or struct initialization.

Use `cargo fmt` after every edit and `cargo clippy` to check your work. There should be no pre-existing clippy errors: even if you think an error is unrelated to your change, fix it right away.

Be liberal with rustdoc: show how the documented function or struct fits into the overall system, and requirements that aren't expressed in the type system (e.g. passing -1 for an integer argument has a special meaning).