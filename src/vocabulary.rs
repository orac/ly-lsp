//! The set of commands ly-lsp recognises.
//!
//! LilyPond's built-in commands are parsed from its `lilypond-words` file; on
//! top of that sit our own special cases — commands the words file omits, and
//! the CamelCase context-reference convention. Layered underneath is the
//! hand-written [`command`] table, which answers not just "is `\foo` known?"
//! but "what does it do?" for the keyword commands (`\repeat`, `\relative`,
//! `\set`, …); see [`doc/command-parsing.md`](../doc/command-parsing.md) for
//! the fuller design, including the `workspace`/`install` layers this module
//! grows later.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock};

use crate::command::{self, Command, Param};

/// Commands that are valid but absent from `lilypond-words`, so we supply them
/// ourselves. `discant` is defined in Scheme by
/// `#(use-modules (lily accreg))`; we don't parse that definition, so we name
/// the command explicitly rather than chase it through the module.
const EXTRA_COMMANDS: &[&str] = &["discant"];

/// A command known only by name — a `lilypond-words` entry with nothing
/// hand-written behind it. [`Vocabulary::get`] hands out one shared instance
/// for every such name: every method but `signature` is already the trait's
/// default (an empty signature, an inherited music context, no documentation),
/// so nothing distinguishes one instance from another, and nothing reads
/// `name` off it — `is_known` and the callers that matter answer from the
/// name they looked up, not from what comes back.
struct Placeholder;

impl Command for Placeholder {
    fn name(&self) -> &str {
        ""
    }

    fn signature(&self) -> &[Param] {
        &[]
    }
}

static PLACEHOLDER: LazyLock<Arc<dyn Command>> = LazyLock::new(|| Arc::new(Placeholder));

/// The commands ly-lsp knows about, resolved in priority order: hand-written
/// [`builtin`](Self::builtin) impls, then `workspace`/`install` definitions
/// (not populated before step 3/4), then bare `known_names` from
/// `lilypond-words` with nothing behind them.
///
/// Context references (`\Staff`, `\PianoStaff`, and user-defined contexts) are
/// recognised by their CamelCase initial rather than stored here: the words
/// file lists context names without a backslash, indistinguishable from the
/// grob and engraver names we *don't* want to accept as commands.
#[derive(Default)]
pub struct Vocabulary {
    /// Definitions from the user's files. Rebuilt when a defining document
    /// changes. Empty until step 3.
    workspace: HashMap<String, Arc<dyn Command>>,
    /// Read from the active LilyPond install. Empty until step 4.
    install: HashMap<String, Arc<dyn Command>>,
    /// Names from `lilypond-words` with nothing behind them. Known, but with
    /// an empty signature.
    known_names: HashSet<String>,
}

// `Command` carries no `Debug` bound (it's an object-safe trait for dynamic
// dispatch, kept minimal), so `Arc<dyn Command>` isn't `Debug` either and the
// three command maps can't be derived. `DocumentGraph` derives `Debug` and
// holds a `Vocabulary`, so this stands in with the shape that matters for
// diagnosing a stuck server: how many names are known, not what each one does.
impl std::fmt::Debug for Vocabulary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vocabulary")
            .field("builtin", &command::BUILTIN.len())
            .field("workspace", &self.workspace.len())
            .field("install", &self.install.len())
            .field("known_names", &self.known_names.len())
            .finish()
    }
}

impl Vocabulary {
    /// Loads the vocabulary from a `lilypond-words` file, folding in our extras.
    ///
    /// Returns `None` if the file can't be read, so the caller can leave
    /// undefined-reference diagnostics switched off rather than flag everything.
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        Some(Self::from_words(&text))
    }

    /// Builds a vocabulary from the contents of a `lilypond-words` file.
    fn from_words(text: &str) -> Self {
        let mut known_names = parse_words(text);
        known_names.extend(EXTRA_COMMANDS.iter().map(|s| (*s).to_string()));
        Self {
            workspace: HashMap::new(),
            install: HashMap::new(),
            known_names,
        }
    }

    /// The command `\name` refers to, resolved `builtin` → `workspace` →
    /// `install` → a nameless placeholder for a bare `known_names` entry, or
    /// `None` if `name` is recognised nowhere. `builtin` is
    /// [`command::BUILTIN`] itself rather than a layer of `Vocabulary`'s own,
    /// so this and [`command::parse`] agree by construction.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Command>> {
        command::BUILTIN
            .get(name)
            .or_else(|| self.workspace.get(name))
            .or_else(|| self.install.get(name))
            .or_else(|| self.known_names.contains(name).then(|| &*PLACEHOLDER))
    }

    /// Whether `\name` is a command we recognise. `name` is the command without
    /// its leading backslash.
    ///
    /// CamelCase names are accepted unconditionally: by LilyPond convention a
    /// `\Foo` with an uppercase initial is a context reference (a built-in like
    /// `\Staff` or a user-defined context), which the words file doesn't carry
    /// as a command. The price is that a mistyped context name goes unflagged.
    pub fn is_known(&self, name: &str) -> bool {
        is_context_reference(name) || self.get(name).is_some()
    }
}

/// Whether `name` looks like a context reference, i.e. its first character is an
/// uppercase letter.
fn is_context_reference(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// Parses a `lilypond-words` file into command names. Command entries carry a
/// doubled leading backslash (`\\relative`); context, grob and engraver names
/// (`Staff`, `NoteHead`, `Note_heads_engraver`) have none and are dropped.
fn parse_words(text: &str) -> HashSet<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix(r"\\"))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_drops_context_names() {
        // Commands carry a doubled backslash and are kept with both stripped;
        // context/grob names without a backslash are dropped.
        let words = "\\\\relative\n\\\\new\nStaff\nNoteHead\n\\\\score\n";
        let commands = parse_words(words);
        assert!(commands.contains("relative"));
        assert!(commands.contains("new"));
        assert!(commands.contains("score"));
        assert!(!commands.contains("Staff"));
        assert!(!commands.contains("NoteHead"));
        assert_eq!(commands.len(), 3);
    }

    #[test]
    fn extras_are_known_even_when_absent_from_words() {
        let vocab = Vocabulary::from_words("\\\\relative\n");
        assert!(vocab.is_known("relative"));
        assert!(vocab.is_known("discant"));
    }

    #[test]
    fn builtin_commands_are_known_even_when_absent_from_words_and_extras() {
        // `with` used to need listing among the extras; now it's answered by
        // the builtin table instead, without appearing in either source.
        let vocab = Vocabulary::from_words("\\\\relative\n");
        assert!(vocab.is_known("with"));
        assert!(vocab.get("with").is_some());
    }

    #[test]
    fn camelcase_commands_are_accepted_as_context_references() {
        let vocab = Vocabulary::from_words("\\\\relative\n");
        // Built-in and user-defined contexts alike, without being in the words.
        assert!(vocab.is_known("Staff"));
        assert!(vocab.is_known("MyOwnContext"));
        // Lowercase commands still have to be known.
        assert!(!vocab.is_known("wibble"));
    }

    #[test]
    fn get_resolves_a_builtin_command() {
        let vocab = Vocabulary::from_words("");
        let repeat = vocab.get("repeat").expect("repeat is builtin");
        assert_eq!(repeat.name(), "repeat");
        assert_eq!(repeat.signature().len(), 3);
    }

    #[test]
    fn get_resolves_a_bare_known_name_to_an_empty_signature() {
        let vocab = Vocabulary::from_words("\\\\break\n");
        let placeholder = vocab.get("break").expect("break is a known name");
        assert!(placeholder.signature().is_empty());
    }

    #[test]
    fn get_returns_none_for_an_unknown_name() {
        let vocab = Vocabulary::from_words("\\\\relative\n");
        assert!(vocab.get("wibble").is_none());
    }
}
