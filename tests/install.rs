//! Tests for the install layer against real LilyPond installations: see
//! `TESTING.md` for how they're found, and "The install layer" in
//! `doc/command-parsing.md` for what this layer is and why.

mod common;

use common::require_installs;
use ly_lsp::command;
use ly_lsp::install;
use ly_lsp::vocabulary;

/// The fixed file list is only staleness-proof if a version that renames or
/// drops one of these files fails a test loudly, rather than quietly losing
/// a few hundred commands.
#[test]
fn every_listed_file_exists_in_every_install() {
    for lily in require_installs() {
        let ly_dir = lily.share_dir().join("ly");
        for file in install::file_names() {
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
                .unwrap_or_else(|| panic!("LilyPond {}: \\{name} should resolve", lily.version));
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
            .unwrap_or_else(|| panic!("LilyPond {}: \\absolute should resolve", lily.version));
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
/// reason a version bump — but several hundred names across seventeen files
/// is the whole point of this layer, so a count anywhere near zero means the
/// reading broke.
#[test]
fn the_install_layer_clears_a_sensible_lower_bound() {
    for lily in require_installs() {
        let layer = install::load(&lily.share_dir().join("ly"));
        assert!(
            layer.len() >= 300,
            "LilyPond {}: only {} commands in the install layer, expected several hundred",
            lily.version,
            layer.len()
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
        let layer = install::load(&lily.share_dir().join("ly"));

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
