//! The note-name languages, read from a real installation.
//!
//! `src/note_names.rs` unit-tests the reader against a fixture holding one language, English and twelve-tone, which is enough for a test that just needs to write a note. Everything that depends on what LilyPond actually ships — how many languages there are, what they spell, the aliases, the quarter tones, and switching between them — is here, against every installation the machine has, so a version that reorganises `define-note-names.scm` fails loudly rather than quietly losing every note name.

mod common;

use common::{install_base, require_installs};
use ly_lsp::command::{Candidate, CompletionContext};
use ly_lsp::note_analyser::analyse;
use ly_lsp::note_names::NoteNames;
use ly_lsp::notes::{EventKind, NoteAnalysis, Problem};
use ly_lsp::vocabulary::Scope;

/// Every language LilyPond has shipped for as long as this server has supported it. Named rather than counted, so that a version dropping one says which.
const EXPECTED_LANGUAGES: &[&str] = &[
    "catalan",
    "català",
    "deutsch",
    "english",
    "espanol",
    "español",
    "français",
    "italiano",
    "nederlands",
    "norsk",
    "portugues",
    "português",
    "suomi",
    "svenska",
    "vlaams",
];

fn note_names_of(install: &common::LilyPondInstall) -> NoteNames {
    let names = NoteNames::read(&install.share_dir());
    assert!(
        !names.is_empty(),
        "LilyPond {}: no note-name language could be read at all",
        install.version
    );
    names
}

#[test]
fn every_installation_ships_the_languages_we_expect() {
    for install in require_installs() {
        let names = note_names_of(&install);
        for expected in EXPECTED_LANGUAGES {
            assert!(
                names.language(expected).is_some(),
                "LilyPond {}: no `{expected}` note-name language",
                install.version
            );
        }
    }
}

#[test]
fn the_default_language_is_dutch() {
    for install in require_installs() {
        let names = note_names_of(&install);
        let default = names.default_language();
        assert_eq!(
            default.name(),
            "nederlands",
            "LilyPond {}: the language in force before any \\language",
            install.version
        );
        // The spellings that make Dutch Dutch: `is` sharpens, `es` flattens, and `b` is B natural rather than B flat.
        assert_eq!(default.note("cis"), Some((0, 2)), "on {}", install.version);
        assert_eq!(default.note("ees"), Some((2, -2)), "on {}", install.version);
        assert_eq!(default.note("b"), Some((6, 0)), "on {}", install.version);
    }
}

#[test]
fn each_language_spells_its_own_naturals() {
    for install in require_installs() {
        let names = note_names_of(&install);
        for (language, naturals) in [
            ("english", ["c", "d", "e", "f", "g", "a", "b"]),
            // German and its neighbours call B natural `h`.
            ("deutsch", ["c", "d", "e", "f", "g", "a", "h"]),
            ("italiano", ["do", "re", "mi", "fa", "sol", "la", "si"]),
        ] {
            let language = names.language(language).expect(language);
            assert_eq!(
                language.naturals(),
                naturals,
                "LilyPond {}: {} naturals",
                install.version,
                language.name()
            );
        }
    }
}

#[test]
fn an_alias_resolves_to_the_language_it_aliases() {
    for install in require_installs() {
        let names = note_names_of(&install);
        // The ASCII spellings LilyPond adds for the accented names, and the
        // one pseudo-language: `semi-german` reads exactly as `deutsch` does.
        for (alias, canonical) in [
            ("catalan", "català"),
            ("espanol", "español"),
            ("portugues", "português"),
        ] {
            assert_eq!(
                names
                    .language(alias)
                    .map(|language| language.name().to_string()),
                Some(canonical.to_string()),
                "LilyPond {}: `{alias}` should alias `{canonical}`",
                install.version
            );
        }
    }
}

#[test]
fn quarter_tones_are_read_as_well_as_semitones() {
    for install in require_installs() {
        let names = note_names_of(&install);
        let english = names.language("english").expect("english");
        // In quarter-tone steps: a semitone is 2, so a quarter-tone sharp is
        // 1 and three quarter tones flat is -3.
        assert_eq!(english.note("cqs"), Some((0, 1)), "on {}", install.version);
        assert_eq!(
            english.note("ctqf"),
            Some((0, -3)),
            "on {}",
            install.version
        );
    }
}

#[test]
fn the_shortest_spelling_wins_where_a_language_offers_several() {
    for install in require_installs() {
        let names = note_names_of(&install);
        let english = names.language("english").expect("english");
        // English names E flat both `ef` and `e-flat`; a refactoring that
        // writes a pitch out should choose the terse form a writer would.
        assert_eq!(english.spell(2, -2), Some("ef"), "on {}", install.version);
        assert_eq!(english.spell(0, 2), Some("cs"), "on {}", install.version);
        assert_eq!(
            english.spell(2, 0),
            Some("e"),
            "LilyPond {}: a natural keeps its bare letter rather than `e-natural`",
            install.version
        );
    }
}

#[test]
fn every_spelling_round_trips_through_spell_and_note() {
    for install in require_installs() {
        let names = note_names_of(&install);
        for (_, language) in names.accepted_names() {
            for note_name in 0..7 {
                for alteration in -4..=4 {
                    let Some(spelling) = language.spell(note_name, alteration) else {
                        continue;
                    };
                    assert_eq!(
                        language.note(spelling),
                        Some((note_name, alteration)),
                        "LilyPond {}: {} spells ({note_name}, {alteration}) as {spelling:?}, which reads back as something else",
                        install.version,
                        language.name()
                    );
                }
            }
        }
    }
}

/// The pitches and problems of `src`, analysed against a real installation.
fn analysed(src: &str) -> NoteAnalysis {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
        .expect("load grammar");
    let tree = parser.parse(src, None).expect("parse");
    analyse(&tree, src, &install_base().for_document(&[]))
}

fn alterations(analysis: &NoteAnalysis) -> Vec<i8> {
    analysis
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Note { pitch, .. } => Some(pitch.alteration),
            _ => None,
        })
        .collect()
}

#[test]
fn a_language_directive_switches_the_note_names() {
    // `cs` and `ef` are English; in the default Dutch they are no notes at all.
    let analysis = analysed("\\language \"english\" { cs ef }");
    assert!(analysis.problems.is_empty(), "{:?}", analysis.problems);
    assert_eq!(alterations(&analysis), vec![2, -2]);
}

#[test]
fn an_unquoted_language_directive_is_honoured() {
    let analysis = analysed("\\language english { ef bf }");
    assert!(analysis.problems.is_empty(), "{:?}", analysis.problems);
    assert_eq!(alterations(&analysis), vec![-2, -2]);
}

#[test]
fn including_a_language_file_switches_the_note_names() {
    // LilyPond's `english.ly` is a one-line `\language "english"` shim, so
    // including it selects the language exactly as saying so would.
    let analysis = analysed("\\include \"english.ly\" { cs ef }");
    assert!(analysis.problems.is_empty(), "{:?}", analysis.problems);
    assert_eq!(alterations(&analysis), vec![2, -2]);
}

#[test]
fn a_language_switch_outlives_the_block_it_was_written_in() {
    // LilyPond's parser switches note names for the rest of the parse; a
    // closing brace doesn't put the old ones back.
    let analysis = analysed("{ \\language \"english\" cs } { ef }");
    assert!(analysis.problems.is_empty(), "{:?}", analysis.problems);
    assert_eq!(alterations(&analysis), vec![2, -2]);
}

#[test]
fn a_language_nobody_ships_leaves_the_names_alone() {
    // The directive is parsed, the language isn't found, and Dutch stays in
    // force — so `cis` still reads and `cs` still doesn't.
    let analysis = analysed("\\language \"klingon\" { cis cs }");
    assert_eq!(alterations(&analysis), vec![2]);
    assert!(
        analysis
            .problems
            .iter()
            .any(|problem| matches!(problem, Problem::NotANote(_))),
        "`cs` is no Dutch note name: {:?}",
        analysis.problems
    );
}

/// The candidates a command offers at parameter `index`, resolved through a scope built on a real installation.
fn candidates_for(command: &str, index: usize) -> Vec<Candidate> {
    let scope = install_base().for_document(&[]);
    let known = scope.get(command).expect(command);
    known.value.completions(
        index,
        &CompletionContext {
            lilypond_version: None,
            scope: &scope,
        },
    )
}

#[test]
fn language_completion_offers_every_name_the_installation_accepts() {
    let candidates = candidates_for("language", 0);
    let labels: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.label.as_ref())
        .collect();
    for expected in EXPECTED_LANGUAGES {
        assert!(
            labels.contains(expected),
            "no `{expected}` among the \\language completions: {labels:?}"
        );
    }
    assert!(
        labels.windows(2).all(|pair| pair[0] <= pair[1]),
        "the names should be offered in alphabetical order: {labels:?}"
    );
    // Each is described by its own naturals, which is what tells `deutsch`
    // from `english` at a glance.
    let deutsch = candidates
        .iter()
        .find(|candidate| candidate.label == "deutsch")
        .expect("deutsch");
    assert_eq!(deutsch.documentation, "Note names: c, d, e, f, g, a, h.");
}

#[test]
fn include_completion_does_not_offer_language_names() {
    // An `\include`'s argument is a filename that only sometimes happens to
    // name a language shim; a dozen of them would bury the common case.
    let labels: Vec<String> = candidates_for("include", 0)
        .into_iter()
        .map(|candidate| candidate.label.into_owned())
        .collect();
    assert!(
        !labels.iter().any(|label| label == "nederlands"),
        "{labels:?}"
    );
}

#[test]
fn with_no_installation_nothing_is_flagged_as_not_a_note() {
    // The degraded path, the counterpart of `is_known`'s CamelCase fallback
    // in `install.rs`: knowing no spellings, we are in no position to say a
    // symbol isn't one, so we say nothing rather than flagging the score.
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
        .expect("load grammar");
    let src = "{ c cis wibble }";
    let tree = parser.parse(src, None).expect("parse");
    let analysis = analyse(&tree, src, &Scope::builtins_only());
    assert!(
        analysis.problems.is_empty(),
        "with no note names loaded, nothing can be judged: {:?}",
        analysis.problems
    );
}
