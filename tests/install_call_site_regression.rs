//! Regression test for [`Commands::call_site_at`] against the *install*
//! layer's signatures, alongside `install_note_regression.rs`'s differential
//! checks: see "The risk: several hundred signatures arriving at once" in
//! `doc/command-parsing.md`.
//!
//! `\time`'s signature — `[beat-structure] fraction` — comes from reading a
//! real installation rather than a curated row (see `TESTING.md`), so this
//! has to run against one too.

mod common;

use common::require_installs;
use ly_lsp::note_analyser::analyse;
use ly_lsp::vocabulary;

fn tree(src: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
        .expect("load grammar");
    parser.parse(src, None).expect("parse")
}

/// Once `\time 4/4` has matched both its parameters, the call site must not
/// reach past its trailing space onto the note that follows: `4/4` is one
/// `fraction` token, not two comma-separated numbers, so it can never have
/// landed in the optional `beat-structure` slot, and with every parameter
/// already filled there is nothing left for `\time` to say about `c4`.
#[test]
fn a_fully_matched_time_signature_does_not_reach_onto_the_following_note() {
    for lily in require_installs() {
        let base = vocabulary::workspace_base(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: words file didn't load: {err}", lily.version)
        });
        let scope = base.for_document(&[]);

        let src = "{ \\time 4/4 c4 }";
        let commands = analyse(&tree(src), src, &scope).commands;

        let offset = src.find("c4").unwrap();
        assert!(
            commands.call_site_at(offset, src).is_none(),
            "LilyPond {}: \\time's call site should not cover the note that follows it",
            lily.version,
        );
    }
}
