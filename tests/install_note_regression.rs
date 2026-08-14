//! Differential regression tests for note analysis against the *install*
//! layer's signatures: see "The risk: several hundred signatures arriving at
//! once" in `doc/command-parsing.md`.
//!
//! Every command that reads notes has to run against a real installation's
//! vocabulary, since the whole point is that these signatures now come from
//! there rather than nowhere — see `TESTING.md`. Each test compares analysing
//! a snippet that uses the command against analysing the same music with the
//! command (and only the command) removed; a difference means an argument's
//! `ArgKind` swallowed a note, or shifted a `\relative` reference, that it
//! shouldn't have.

mod common;

use common::require_installs;
use ly_lsp::note_analyser::analyse;
use ly_lsp::notes::{EventKind, NoteAnalysis};
use ly_lsp::vocabulary;

fn tree(src: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
        .expect("load grammar");
    parser.parse(src, None).expect("parse")
}

/// The resolved pitches of every `Note` event, as `(note_name, octave)`.
fn pitches(analysis: &NoteAnalysis) -> Vec<(u8, i32)> {
    analysis
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Note { pitch, .. } => Some((pitch.note_name, pitch.octave)),
            _ => None,
        })
        .collect()
}

/// The resolved durations of every event, as `(log, dots)`.
fn durations(analysis: &NoteAnalysis) -> Vec<(i32, u8)> {
    analysis
        .events
        .iter()
        .map(|e| (e.duration.log, e.duration.dots))
        .collect()
}

/// Checks that `with` and `without` analyse to the same pitches and
/// durations, against every installation found — the install layer's
/// vocabulary is what supplies the signature under test, so this can't be
/// checked with the bare-builtin scope the rest of the note analyser's own
/// tests use.
fn assert_same_events(with: &str, without: &str) {
    for lily in require_installs() {
        let base = vocabulary::workspace_base(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: words file didn't load: {err}", lily.version)
        });
        let scope = base.for_document(&[]);

        let with_tree = tree(with);
        let with_analysis = analyse(&with_tree, with, &scope);
        let without_tree = tree(without);
        let without_analysis = analyse(&without_tree, without, &scope);

        assert_eq!(
            pitches(&with_analysis),
            pitches(&without_analysis),
            "LilyPond {}: pitches differ between {with:?} and {without:?}",
            lily.version,
        );
        assert_eq!(
            durations(&with_analysis),
            durations(&without_analysis),
            "LilyPond {}: durations differ between {with:?} and {without:?}",
            lily.version,
        );
    }
}

/// `\tweak`'s real signature (`key-list-or-symbol? scheme? ly:music?`) ends
/// in a plain `ly:music?`, already mapped — no ambiguity to speak of, unlike
/// the doc's worst-case sketch. This pins that the note it tweaks is still
/// analysed, not merely skipped over.
#[test]
fn tweak_still_walks_its_music() {
    assert_same_events("{ \\tweak Stem.color #red c4 d4 }", "{ c4 d4 }");
}

#[test]
fn once_still_walks_its_music() {
    assert_same_events("{ \\once { c4 d4 } }", "{ { c4 d4 } }");
}

/// `\grace`/`\acciaccatura`/`\appoggiatura` are built with `def-grace-function`,
/// not a `define-…-function` this reader recognises (see `install.rs`'s own
/// test of the same fact), so they resolve to a bare, argument-less binding —
/// exactly as before this layer had any signatures at all. Pinned here from
/// the note-analysis side: the block that follows must still be read as
/// ordinary music.
#[test]
fn grace_family_still_lets_the_following_block_be_read_as_music() {
    for keyword in ["grace", "acciaccatura", "appoggiatura"] {
        assert_same_events(&format!("{{ \\{keyword} {{ c4 d4 }} }}"), "{ { c4 d4 } }");
    }
}

#[test]
fn grace_family_does_not_perturb_the_relative_reference() {
    for keyword in ["grace", "acciaccatura", "appoggiatura"] {
        assert_same_events(
            &format!("\\relative c' {{ \\{keyword} {{ g4 }} }}"),
            "\\relative c' { { g4 } }",
        );
    }
}

/// `afterGrace = (fraction main grace) ((scale?) ly:music? ly:music?)`: the
/// fraction is almost always omitted in real scores. Before the general
/// `ArgKind::Unknown` fix, the optional (and unmapped) `scale?` slot
/// unconditionally consumed whatever came next — here, `main` itself — and
/// the note inside it vanished from the analysis instead of being walked.
#[test]
fn after_grace_omitting_its_optional_fraction_still_walks_main_and_grace() {
    assert_same_events("{ \\afterGrace c1 { d8 } }", "{ c1 { d8 } }");
}

#[test]
fn after_grace_does_not_perturb_the_relative_reference() {
    assert_same_events(
        "\\relative c' { \\afterGrace g1 { a8 } }",
        "\\relative c' { g1 { a8 } }",
    );
}

#[test]
fn parenthesize_still_walks_its_argument() {
    assert_same_events("{ \\parenthesize c4 d4 }", "{ c4 d4 }");
}

/// `shape = (offsets item) (list? key-list-or-music?)`. `offsets` is always a
/// Scheme list literal (`#'(...)`, one `embedded_scheme` node), so the
/// unmapped, required `list?` never has an optional-consumes-the-music
/// problem; `item`, mapped to `Music`, still needs to be walked rather than
/// merely spanned.
#[test]
fn shape_still_walks_its_item() {
    assert_same_events("{ \\shape #'((0 . 0) (0.5 . 0)) c4 d4 }", "{ c4 d4 }");
}

/// `footnote = (mark offset footnote item) ((markup?) number-pair? markup? symbol-list-or-music?)`.
/// `mark` is almost always omitted, which is exactly the shape that used to
/// let an unmapped optional predicate eat a required argument out of turn —
/// here it would have consumed `offset`'s own value. The general fix means
/// the arguments after `mark` may still misalign, but `item`'s note must not
/// be lost as a result.
#[test]
fn footnote_omitting_its_optional_mark_still_walks_its_item() {
    assert_same_events("{ \\footnote #'(1 . 2) \"see below\" c4 d4 }", "{ c4 d4 }");
}

/// `autoChange = (pitch clef-1 clef-2 music) ((ly:pitch?) (ly:context-mod?) (ly:context-mod?) ly:music?)`.
/// Every one of `pitch`, `clef-1` and `clef-2` is optional, and real scores
/// almost always give none of them (`\autoChange { music }`). Before the
/// general fix, the first optional `ly:context-mod?` — unmapped, and so
/// `Unknown` — would have swallowed the whole music block whole.
#[test]
fn auto_change_omitting_all_its_optional_arguments_still_walks_its_music() {
    assert_same_events("{ \\autoChange { c d e } }", "{ { c d e } }");
}

/// `tuplet = (ratio tuplet-span music) (fraction? (ly:duration? '()) ly:music?)`.
/// `tuplet-span` is optional and almost never given
/// (`\tuplet 3/2 { c d e }` is the form every real score uses); before the
/// general `ArgKind::Unknown` fix this was the single most damaging case in
/// this file, since the unmapped, optional `ly:duration?` slot would consume
/// the tuplet's entire body.
#[test]
fn tuplet_omitting_its_optional_span_still_walks_its_music() {
    assert_same_events("{ \\tuplet 3/2 { c4 d4 e4 } }", "{ { c4 d4 e4 } }");
}

/// The old spelling of `\tuplet`, kept for backward compatibility and
/// defined the same way.
#[test]
fn times_still_walks_its_music() {
    assert_same_events("{ \\times 2/3 { c4 d4 e4 } }", "{ { c4 d4 e4 } }");
}

#[test]
fn bar_does_not_affect_the_following_notes() {
    assert_same_events("{ c4 \\bar \"|.\" d4 }", "{ c4 d4 }");
}

/// `mark = (label) ((index-or-markup?))`: `label` is optional and, written
/// bare (`\mark` with nothing after it that could be a label), the very next
/// token is the note that follows in the music. Before the general fix an
/// unmapped optional predicate consumed unconditionally, so that note would
/// have been read as `\mark`'s own (nonsensical) label instead of being
/// analysed.
#[test]
fn mark_omitting_its_optional_label_still_walks_the_following_note() {
    assert_same_events("{ \\mark c4 d4 }", "{ c4 d4 }");
}

/// `skip = (arg) (duration-or-music?)`: `arg` is nearly always a bare
/// duration (`\skip 4`), not music. Mapping `duration-or-music?` to
/// `ArgKind::Music` (the safe choice for the rarer music-argument form) means
/// `\skip 4` fails to match at all — this pins that the unconsumed duration
/// digit is then simply ignored rather than corrupting the duration a
/// following bare note inherits, mirroring the existing `\tempo` pin.
#[test]
fn skip_with_a_bare_duration_does_not_affect_the_following_notes_inherited_duration() {
    assert_same_events("{ c8 \\skip 4 d }", "{ c8 d }");
}

/// The rarer written form, `\skip` given music directly, must still be
/// walked rather than swallowed as `Unknown`.
#[test]
fn skip_with_music_still_walks_it() {
    assert_same_events("{ \\skip { c4 d4 } }", "{ { c4 d4 } }");
}
