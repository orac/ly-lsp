//! Measures how long [`vocabulary::workspace_base`] takes against a real installation,
//! and how many commands its install layer ends up with — the number
//! `doc/command-parsing.md`'s "When it happens" section asks to be measured
//! before reaching for anything more complex than doing this synchronously.
//!
//! ```text
//! cargo run --release --example install_timing -- "<share>/lilypond/<version>"
//! ```

use std::time::Instant;

use ly_lsp::{install, vocabulary};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| panic!("usage: install_timing <path to share/lilypond/<version>>"));

    let share_dir = std::path::Path::new(&path);

    let start = Instant::now();
    let base = vocabulary::workspace_base(share_dir)
        .unwrap_or_else(|err| panic!("could not load {path}: {err}"));
    let elapsed = start.elapsed();

    println!("loaded in {elapsed:?}");
    // Top down: our curated signatures, the install, the words list.
    // `Layer::len` counts both namespaces a layer can hold — commands and
    // context types together — so "names" is what these numbers are.
    for layer in base.layers() {
        println!("  a layer of {} names", layer.len());
    }
    println!(
        "install layer alone defines {} names",
        install::load(&share_dir.join("ly")).len()
    );
}
