//! Tests for the install layer against real LilyPond installations: see
//! `TESTING.md` for how they're found, and "The install layer" in
//! `doc/command-parsing.md` for what this layer is and why.

mod common;

use common::require_installs;
use ly_lsp::command;
use ly_lsp::document_graph::DocumentGraph;
use ly_lsp::install;
use ly_lsp::note_analyser::analyse;
use ly_lsp::notes::EventKind;
use ly_lsp::vocabulary;
use tower_lsp::lsp_types::{Position, Url};

fn tree(src: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
        .expect("load grammar");
    parser.parse(src, None).expect("parse")
}

/// The fixed file list is only staleness-proof if a version that renames or
/// drops one of these files fails a test loudly, rather than quietly losing
/// a few hundred commands.
#[test]
fn every_listed_file_exists_in_every_install() {
    for lily in require_installs() {
        let ly_dir = lily.share_dir().join("ly");
        for file in install::file_names()
            .iter()
            .chain(install::context_file_names())
        {
            assert!(
                ly_dir.join(file).is_file(),
                "LilyPond {}: expected {} to exist under {}",
                lily.version,
                file,
                ly_dir.display()
            );
        }
    }
}

/// A named handful the install layer must define, each with the arity real
/// LilyPond gives it. All five are LilyPond's own zero-argument shorthands —
/// postfix articulations, a dynamic mark, a property setter, a line-break
/// event — so `\accent`, `\pp`, `\slurUp` and `\break` are used with no
/// argument in real scores, and the install layer resolving them to a command
/// with an empty signature (rather than merely a known name) is what lets
/// them be told apart from ordinary music. `\appoggiatura` is checked only
/// for being known: its real definition goes through `def-grace-function`
/// rather than `define-music-function` directly, which the reader doesn't
/// evaluate its way through (see "Read, don't evaluate" in the doc), so it
/// resolves to a plain, argument-less binding today — an accepted limitation
/// item 2 of "Order of work" may narrow, not a figure worth pinning here.
#[test]
fn the_layer_defines_the_documented_handful() {
    for lily in require_installs() {
        let base = vocabulary::workspace_base(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: words file didn't load: {err}", lily.version)
        });
        let scope = base.for_document(&[]);

        for name in ["appoggiatura", "accent", "pp", "slurUp", "break"] {
            assert!(
                scope.is_known(name),
                "LilyPond {}: \\{name} should be known",
                lily.version
            );
        }

        for name in ["accent", "pp", "slurUp", "break"] {
            let command = scope
                .get(name)
                .unwrap_or_else(|| panic!("LilyPond {}: \\{name} should resolve", lily.version))
                .value;
            assert!(
                command.signature().is_empty(),
                "LilyPond {}: \\{name} should take no arguments, got {:?}",
                lily.version,
                command.signature()
            );
        }
    }
}

/// `\absolute`, from `music-functions-init.ly`, is a `define-music-function`
/// with a docstring — `(_i "Make @var{music} absolute. …")` — of the kind the
/// scheme reader already reads for a user's own files, proving the install
/// layer carries documentation through and not just names and arities.
#[test]
fn a_documented_music_function_carries_its_docstring_and_signature() {
    for lily in require_installs() {
        let base = vocabulary::workspace_base(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: words file didn't load: {err}", lily.version)
        });
        let scope = base.for_document(&[]);

        let absolute = scope
            .get("absolute")
            .unwrap_or_else(|| panic!("LilyPond {}: \\absolute should resolve", lily.version))
            .value;
        assert_eq!(
            absolute
                .signature()
                .iter()
                .map(|p| p.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["music"],
            "LilyPond {}",
            lily.version
        );
        assert!(
            absolute.documentation().is_some(),
            "LilyPond {}: \\absolute should carry a docstring",
            lily.version
        );
    }
}

/// Not an exact figure — versions differ, and pinning one would fail for no
/// reason a version bump — but several hundred commands and context types
/// across nineteen files is the whole point of this layer, so a count
/// anywhere near zero means the reading broke. `Layer::len` counts both
/// namespaces together (see its doc-comment), which is what "entries" means
/// here rather than "commands".
#[test]
fn the_install_layer_clears_a_sensible_lower_bound() {
    for lily in require_installs() {
        let layer = install::load(&lily.share_dir());
        assert!(
            layer.len() >= 300,
            "LilyPond {}: only {} entries in the install layer, expected several hundred",
            lily.version,
            layer.len()
        );
    }
}

/// The two things completion knows only because a real installation is
/// behind it: the number to write in `\version`, and where a name that came
/// out of the install says it came from.
///
/// End to end through the [`DocumentGraph`], because that is where the version
/// is picked out of the share directory the client named, and a version
/// derived correctly but never threaded to the command that offers it would
/// pass any narrower test.
#[test]
fn completion_offers_what_the_installation_says() {
    for lily in require_installs() {
        let ws = DocumentGraph::new();
        ws.load_vocabulary(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: vocabulary didn't load: {err}", lily.version)
        });
        let uri = Url::parse("untitled:score.ly").unwrap();
        ws.open(uri.clone(), "\\version \n{ \\acce }\n".to_string());

        let versions = ws.completions(&uri, Position::new(0, 9));
        let labels: Vec<&str> = versions.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![format!("\"{}\"", lily.version)],
            "LilyPond {}: `\\version ` should complete to the installed version",
            lily.version
        );

        let names = ws.completions(&uri, Position::new(1, 7));
        let accent = names
            .iter()
            .find(|item| item.label == "\\accent")
            .unwrap_or_else(|| panic!("LilyPond {}: \\accent should be offered", lily.version));
        assert_eq!(
            accent.detail.as_deref(),
            Some(format!("lilypond-{}", lily.version).as_str()),
            "LilyPond {}: \\accent comes from the install, and should say so",
            lily.version
        );
    }
}

/// The line between the two hand-written layers, checked against LilyPond
/// itself rather than asserted.
///
/// A name LilyPond defines in `ly/music-functions-init.ly` is an ordinary music
/// function, so a user's file can shadow it and `CURATED` must sit below the
/// file layers. A name it defines nowhere in `ly/` is a reserved word its
/// grammar recognises before any name lookup happens, so nothing can shadow it
/// and `RESERVED` must sit above them. Splitting the table on exactly that
/// property is what lets `Scope` express both with nothing but layer order — so
/// if a future LilyPond moves a name across the line, this is what says so.
#[test]
fn the_hand_written_layers_split_on_what_the_install_defines() {
    for lily in require_installs() {
        let layer = install::load(&lily.share_dir());

        for name in command::CURATED.names() {
            assert!(
                layer.get(name).is_some(),
                "LilyPond {}: `\\{name}` is curated, so it should be an ordinary music \
                 function the install defines — if it is really a reserved word, it \
                 belongs in RESERVED_ROWS",
                lily.version
            );
        }
        for name in command::RESERVED.names() {
            assert!(
                layer.get(name).is_none(),
                "LilyPond {}: `\\{name}` is treated as a reserved word, but the install \
                 defines it — a file can shadow it, so it belongs in CURATED_ROWS",
                lily.version
            );
        }
    }
}

/// We should find these contexts in every install. If not, something went wrong.
#[test]
fn install_declares_common_contexts() {
    for lily in require_installs() {
        let layer = install::load(&lily.share_dir());
        for name in [
            "Staff",
            "Voice",
            "PianoStaff",
            "Lyrics",
            "ChordNames",
            "StaffGroup",
        ] {
            assert!(
                layer.get_context_type(name).is_some(),
                "LilyPond {}: context type `{name}` should be declared by the install",
                lily.version
            );
        }
    }
}

/// The critical case the merge exists for: `Staff` is declared once in
/// `engraver-init.ly`, with a `\description`, and again in
/// `performer-init.ly`, without one. `load` must not let the second,
/// description-less declaration blank the first out.
#[test]
fn staff_keeps_its_description_through_the_engraver_performer_merge() {
    for lily in require_installs() {
        let layer = install::load(&lily.share_dir());
        let staff = layer
            .get_context_type("Staff")
            .unwrap_or_else(|| panic!("LilyPond {}: Staff should be declared", lily.version));
        let description = staff.description.as_deref().unwrap_or_else(|| {
            panic!(
                "LilyPond {}: Staff should keep its description through the merge",
                lily.version
            )
        });
        assert!(
            description.starts_with("Handles clefs, bar lines, keys, accidentals"),
            "LilyPond {}: unexpected Staff description: {description:?}",
            lily.version
        );
    }
}

/// Each context type binds its own name as a command too, so `\context {
/// \Staff … }` resolves `\Staff` to something real rather than a name the
/// scope has merely heard of.
#[test]
fn a_context_types_name_resolves_as_a_command() {
    for lily in require_installs() {
        let base = vocabulary::workspace_base(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: words file didn't load: {err}", lily.version)
        });
        let scope = base.for_document(&[]);
        assert!(
            scope.get("Staff").is_some(),
            "LilyPond {}: \\Staff should resolve as a command",
            lily.version
        );
    }
}

/// With context types loaded, the CamelCase shape rule is no longer a
/// blanket pass: a plausible typo of a real context name must be flagged.
#[test]
fn is_known_rejects_a_typo_when_context_types_are_loaded() {
    for lily in require_installs() {
        let base = vocabulary::workspace_base(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: words file didn't load: {err}", lily.version)
        });
        let scope = base.for_document(&[]);
        assert!(
            !scope.is_known("Vioce"),
            "LilyPond {}: `Vioce` is a typo of `Voice`, and should not be known",
            lily.version
        );
        assert!(
            scope.is_known("Voice"),
            "LilyPond {}: `Voice` itself should still be known",
            lily.version
        );
    }
}

/// `\new`/`\context`'s reading of their body depends on the type named —
/// checked here against a real installation's own `ContextType` data, not
/// the hand-maintained root-name fallback `NewContextCommand::music_context`
/// falls back to with no install loaded (see `command::tests` for that
/// fallback, which needs no install at all). `Staff` isn't itself one of the
/// hand-written non-note roots and carries no alias to one either, so this
/// also pins that an ordinary context type still reads its body as note
/// music once real `ContextType` data is behind the lookup.
#[test]
fn new_lyrics_reads_its_body_as_non_note_against_a_real_install() {
    for lily in require_installs() {
        let base = vocabulary::workspace_base(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: words file didn't load: {err}", lily.version)
        });
        let scope = base.for_document(&[]);

        let src = "<< \\new Staff { c } \\new Lyrics { la } >>";
        let analysis = analyse(&tree(src), src, &scope);
        assert!(
            analysis.problems.is_empty(),
            "LilyPond {}: expected no problems in {src:?}, got {:?}",
            lily.version,
            analysis.problems
        );
        let pitches: Vec<(u8, i32)> = analysis
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Note { pitch, .. } => Some((pitch.note_name, pitch.octave)),
                _ => None,
            })
            .collect();
        assert_eq!(
            pitches,
            vec![(0, -1)],
            "LilyPond {}: only the Staff's `c` should be read as a note; `la` inside \\new \
             Lyrics should be non-note and not read at all",
            lily.version
        );
    }
}

/// The degraded path: with no install loaded — `Scope::builtins_only`, the
/// scope with no words layer and no install layer either — the CamelCase
/// shape rule must still accept `\Staff`, because the alternative is
/// flagging every context reference in every score whenever the install
/// can't be read.
#[test]
fn is_known_still_accepts_camel_case_without_an_install() {
    let scope = vocabulary::Scope::builtins_only();
    assert!(
        scope.is_known("Staff"),
        "with no context types loaded, a CamelCase name should still be accepted"
    );
    assert!(
        scope.is_known("Vioce"),
        "the fallback can't tell a typo from a real name — that's the price of degrading safely"
    );
}

/// `\new`/`\context` completion, end to end through the `DocumentGraph`, on
/// the context types a real installation actually declares: a plausible one
/// (`Staff`) is offered, with its `\description` as the item's documentation,
/// and `InternalGregorianStaff` — real in every install these tests find,
/// from 2.24 onward (see `command::is_internal_context_type`'s doc for how
/// that was checked) — is not, even though `\InternalGregorianStaff` is still
/// a resolvable reference (the aliases real Gregorian/Mensural context types
/// build on it through).
#[test]
fn new_offers_real_context_types_and_excludes_internal_ones() {
    for lily in require_installs() {
        let ws = DocumentGraph::new();
        ws.load_vocabulary(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: vocabulary didn't load: {err}", lily.version)
        });
        let uri = Url::parse("untitled:score.ly").unwrap();
        // A bare `\new` with nothing typed after it: the text-level fallback
        // `command_assist::context_argument_completions` exists for, since
        // tree-sitter recovers this as an `ERROR` node rather than a
        // `named_context` (see that function's doc).
        ws.open(uri.clone(), "{ \\new  }\n".to_string());

        let items = ws.completions(&uri, Position::new(0, 7));
        let staff = items
            .iter()
            .find(|item| item.label == "Staff")
            .unwrap_or_else(|| panic!("LilyPond {}: Staff should be offered", lily.version));
        assert!(
            staff
                .detail
                .as_deref()
                .is_some_and(|d| d.starts_with("Handles clefs")),
            "LilyPond {}: Staff's documentation should be its \\description, got {:?}",
            lily.version,
            staff.detail
        );
        assert!(
            !items
                .iter()
                .any(|item| item.label == "InternalGregorianStaff"),
            "LilyPond {}: InternalGregorianStaff must not be offered, got {:?}",
            lily.version,
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );

        let base = vocabulary::workspace_base(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: words file didn't load: {err}", lily.version)
        });
        let scope = base.for_document(&[]);
        assert!(
            scope.get_context_type("InternalGregorianStaff").is_some(),
            "LilyPond {}: InternalGregorianStaff must still be known, just not offered",
            lily.version
        );
    }
}

/// Context instance names, end to end: a `\new Voice = "vocals"` written
/// earlier in the same score is offered — quoted, per the insertion
/// convention `ArgKind::ContextName`'s doc settles on — at a `\lyricsto` with
/// nothing typed after it yet.
#[test]
fn lyricsto_offers_a_context_instance_name_from_earlier_in_the_score() {
    for lily in require_installs() {
        let ws = DocumentGraph::new();
        ws.load_vocabulary(&lily.share_dir()).unwrap_or_else(|err| {
            panic!("LilyPond {}: vocabulary didn't load: {err}", lily.version)
        });
        let uri = Url::parse("untitled:score.ly").unwrap();
        let src = "{ \\new Voice = \"vocals\" { c } \\lyricsto  { la } }\n";
        ws.open(uri.clone(), src.to_string());

        let offset = src.find("\\lyricsto ").unwrap() + "\\lyricsto ".len();
        let position = Position::new(0, offset as u32);
        let items = ws.completions(&uri, position);
        assert!(
            items.iter().any(|item| item.label == "\"vocals\""),
            "LilyPond {}: \"vocals\" should be offered at \\lyricsto, got {:?}",
            lily.version,
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }
}
