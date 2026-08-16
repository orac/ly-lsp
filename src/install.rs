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
use std::path::Path;
use std::sync::Arc;

use crate::command::Command;
use crate::command::variable::Variable;
use crate::context::{self, ContextType};
use crate::document;
use crate::vocabulary::Layer;

/// The files LilyPond's own bootstrap (`ly/declarations-init.ly`, `\include`d
/// from `scm/lily/lily.scm`) pulls in, in that order — the definitions any
/// user file can reach without including anything itself.
///
/// `engraver-init.ly` and `performer-init.ly` appear in [`CONTEXT_FILES`] instead of
/// *this* list: they contain no top-level assignments at all, but are read for their context declarations.
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

/// The files [`load`] reads for [`ContextType`]s rather than commands:
/// `Staff`, `Voice`, `PianoStaff` and every other context type a score can
/// write `\new` or `\context` against.
///
/// LilyPond declares every context type *twice*. `engraver-init.ly` declares
/// it as an engraver context, `\description` and all; `performer-init.ly`
/// declares the same name again as a performer context, with everything
/// but the description repeated. [`load`] doesn't let the second occurrence
/// replace the first outright — that would discard every description, since
/// none of the performer copies carry one — it merges the two field by
/// field; see [`merge_context_type`].
const CONTEXT_FILES: &[&str] = &["engraver-init.ly", "performer-init.ly"];

/// The file names [`load`] reads for commands, in the order it reads them.
/// Exposed so a test can check every one exists in every installation the
/// tests find, without duplicating the list.
pub fn file_names() -> &'static [&'static str] {
    FILES
}

/// The file names [`load`] reads for context types, in the order it reads
/// them. The counterpart of [`file_names`] for [`CONTEXT_FILES`], for the
/// same reason: so a test can assert every one exists in every installation
/// without a second copy of the list to fall out of step.
pub fn context_file_names() -> &'static [&'static str] {
    CONTEXT_FILES
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
/// Also builds the [`ContextType`] namespace from [`CONTEXT_FILES`], merging
/// LilyPond's engraver and performer declarations of each type into one, and
/// binds every context type's name as a command too — a [`Variable`], since
/// `\context { \Staff … }` relies on `\Staff` being a real reference to the
/// `Staff` context definition, exactly as an ordinary zero-argument name
/// would be.
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

    let mut context_types: HashMap<String, ContextType> = HashMap::new();
    for file in CONTEXT_FILES {
        let Ok(text) = std::fs::read_to_string(ly_dir.join(file)) else {
            continue;
        };
        let tree = document::parse(&text, None);
        for found in context::read(&tree, &text) {
            match context_types.remove(&found.name) {
                Some(engraver) => {
                    let name = found.name.clone();
                    context_types.insert(name, merge_context_type(engraver, found));
                }
                None => {
                    context_types.insert(found.name.clone(), found);
                }
            }
        }
    }

    // A context type's own name is a command too: `\Staff` substitutes the
    // `Staff` context definition, just as any other bare name does. A file's
    // own commands win where one happens to collide, on the same "nearer
    // binding wins" principle as everything else `insert`s into this map —
    // though no real name has been observed to collide.
    for name in context_types.keys() {
        commands
            .entry(name.clone())
            .or_insert_with(|| Arc::new(Variable::new(name.clone())) as Arc<dyn Command>);
    }

    Layer::new(origin(ly_dir), commands).with_context_types(context_types)
}

/// Folds `performer`'s declaration of a context type into `engraver`'s,
/// field by field, rather than letting one replace the other outright.
///
/// Named for which file each argument is expected to have come from, since
/// that's what motivates the asymmetry — `engraver-init.ly` carries the
/// `\description`, `performer-init.ly` never does — but the merge itself
/// doesn't assume it: whichever side actually has a description wins, and
/// aliases are the union of both, in `engraver`'s order with any new ones
/// `performer` adds appended. The name and both spans are `engraver`'s,
/// since only one of the two can be kept and there's no reason to prefer the
/// second file's over the first's.
fn merge_context_type(engraver: ContextType, performer: ContextType) -> ContextType {
    let mut aliases = engraver.aliases;
    for alias in performer.aliases {
        if !aliases.contains(&alias) {
            aliases.push(alias);
        }
    }
    ContextType {
        name: engraver.name,
        aliases,
        description: engraver.description.or(performer.description),
        name_span: engraver.name_span,
        block_span: engraver.block_span,
    }
}

/// The version an installation is of, read from the name of its
/// version-specific share directory: `share/lilypond/2.24.3` is `2.24.3`.
///
/// The same layout assumption [`load`] relies on to find [`FILES`] at all, and
/// the one the client makes in passing that directory as `lilypondShareDir`.
/// `None` for a directory not laid out that way, which is honest: we would
/// rather offer no version than a wrong one.
pub fn version(share_dir: &Path) -> Option<&str> {
    share_dir.file_name()?.to_str()
}

/// What hover calls this layer: `lilypond-2.24.3`, naming the version whose
/// files it read. A directory that doesn't name a [`version`] still names
/// LilyPond, just not which one.
fn origin(ly_dir: &Path) -> String {
    match ly_dir.parent().and_then(version) {
        Some(version) => format!("lilypond-{version}"),
        None => "lilypond".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_directory_yields_an_empty_layer_rather_than_failing() {
        let layer = load(Path::new("/does/not/exist/ever/ly"));
        assert!(layer.is_empty());
    }

    #[test]
    fn the_layer_is_named_for_the_version_it_read() {
        assert_eq!(
            origin(Path::new("/usr/share/lilypond/2.24.3/ly")),
            "lilypond-2.24.3"
        );
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
