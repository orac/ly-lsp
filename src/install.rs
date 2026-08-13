//! The install layer: [`Command`] knowledge read out of the active LilyPond
//! installation's own `.ly` initialisation files.
//!
//! `\appoggiatura`, `\accent`, `\pp`, `\slurUp` and several hundred others are
//! defined in exactly the source [`command::scheme`](crate::command::scheme)
//! and [`document`](crate::document) already read for a user's own files —
//! the install files *are* `.ly` files, `foo = #(define-music-function …)`
//! and `foo = #(make-articulation 'foo)` alike. So this module adds no new
//! reading, only new files to point the readers at.
//!
//! See "The install layer" in
//! [`doc/command-parsing.md`](../doc/command-parsing.md) for the fuller
//! design: why this particular file list and no others, and what remains —
//! predicate coverage and the note-analysis regression it risks are item 2
//! of that section's "Order of work", not built here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::command::Command;
use crate::command::definition::Variable;
use crate::document;
use crate::vocabulary::Layer;

/// The files LilyPond's own bootstrap (`ly/declarations-init.ly`, `\include`d
/// from `scm/lily/lily.scm`) pulls in, in that order — the definitions any
/// user file can reach without including anything itself.
///
/// `engraver-init.ly` is deliberately absent: it is `\context { … }` blocks
/// inside a `\layout`, context and engraver defaults rather than commands,
/// and the symbol query only captures top-level assignments, so parsing it
/// would cost 50 KB for almost nothing. Everything else under `ly/` is out
/// too; see doc/command-parsing.md for why.
const FILES: &[&str] = &[
    "declarations-init.ly",
    "music-functions-init.ly",
    "toc-init.ly",
    "drumpitch-init.ly",
    "chord-modifiers-init.ly",
    "script-init.ly",
    "chord-repetition-init.ly",
    "scale-definitions-init.ly",
    "dynamic-scripts-init.ly",
    "spanners-init.ly",
    "predefined-fretboards-init.ly",
    "string-tunings-init.ly",
    "property-init.ly",
    "grace-init.ly",
    "midi-init.ly",
    "paper-defaults-init.ly",
    "context-mods-init.ly",
];

/// The file names [`load`] reads, in the order it reads them. Exposed so a
/// test can check every one exists in every installation the tests find,
/// without duplicating the list.
pub fn file_names() -> &'static [&'static str] {
    FILES
}

/// Builds the install [`Layer`] by reading [`FILES`] out of `ly_dir`, in
/// order, and folding every file's bindings into one map. A name bound more
/// than once — within a file, or by a later file in the list — keeps the
/// last binding, which is LilyPond's own resolution order: it parses these
/// files in this same sequence.
///
/// A file that can't be read (a version whose layout has shifted, a
/// permissions problem, `ly_dir` not existing at all) is skipped rather than
/// failing the whole load: a smaller install layer beats none, and the
/// alternative is refusing every score undefined-reference diagnostics just
/// because one file went missing.
///
/// Built directly rather than through
/// [`definition::layer`](crate::command::definition::layer), which decorates
/// every command with the span of where its file wrote its name. That is
/// right for a layer belonging to one document, and wrong here: the span
/// would carry no file, so go-to-definition on `\appoggiatura` would offer a
/// range in whatever file the cursor happened to be in, wherever that was.
/// So [`Command::definition`] stays `None` for everything this builds —
/// `binding.command` where the reader found one, a bare
/// [`Variable`] where it found only a name — and
/// navigating into the install stays a "Later" item in the doc, waiting on a
/// decorator that carries a file alongside its span.
pub fn load(ly_dir: &Path) -> Layer {
    let mut commands: HashMap<String, Arc<dyn Command>> = HashMap::new();
    for file in FILES {
        let Ok(text) = std::fs::read_to_string(ly_dir.join(file)) else {
            continue;
        };
        for binding in document::parse_bindings(&text) {
            let command = binding.command.unwrap_or_else(|| {
                Arc::new(Variable::new(binding.name.clone())) as Arc<dyn Command>
            });
            commands.insert(binding.name, command);
        }
    }
    Layer::new(commands)
}

/// The `ly` directory of the installation whose `lilypond-words` file is at
/// `words_path`.
///
/// The client passes `lilypondWordsPath` as `<share>/vim/syntax/lilypond-words`
/// at `initialize`, and `ly` is a sibling of `vim` under that same `<share>`
/// directory — `words_path`'s third ancestor. `None` if `words_path` isn't
/// nested that deep, which a well-formed words path always is.
pub fn ly_dir(words_path: &Path) -> Option<PathBuf> {
    Some(words_path.ancestors().nth(3)?.join("ly"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ly_dir_is_the_third_ancestor_joined_with_ly() {
        let words =
            Path::new("/opt/lilypond-2.24.3/share/lilypond/2.24.3/vim/syntax/lilypond-words");
        assert_eq!(
            ly_dir(words),
            Some(PathBuf::from(
                "/opt/lilypond-2.24.3/share/lilypond/2.24.3/ly"
            ))
        );
    }

    #[test]
    fn ly_dir_is_none_for_a_path_too_shallow_to_have_one() {
        assert_eq!(ly_dir(Path::new("lilypond-words")), None);
        assert_eq!(ly_dir(Path::new("syntax/lilypond-words")), None);
    }

    #[test]
    fn a_missing_directory_yields_an_empty_layer_rather_than_failing() {
        let layer = load(Path::new("/does/not/exist/ever/ly"));
        assert!(layer.is_empty());
    }

    #[test]
    fn reads_a_zero_argument_command_from_a_declared_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("declarations-init.ly"),
            "break = #(make-music 'LineBreakEvent 'break-permission 'force)\n",
        )
        .unwrap();
        let layer = load(dir.path());
        let command = layer.get("break").expect("break");
        assert!(command.signature().is_empty());
        assert!(
            command.definition().is_none(),
            "install commands carry no span, on pain of navigating into the wrong file"
        );
    }

    #[test]
    fn a_later_file_in_the_list_wins_over_an_earlier_one() {
        // Same shape as LilyPond's own parse order: later files are read
        // later, so a name both bind resolves to the later file's version.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("declarations-init.ly"),
            "dup = #(define-music-function (m) (ly:music?) m)\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("music-functions-init.ly"),
            "dup = #(define-music-function (a b) (ly:music? ly:music?) a)\n",
        )
        .unwrap();
        let layer = load(dir.path());
        assert_eq!(layer.get("dup").expect("dup").signature().len(), 2);
    }
}
