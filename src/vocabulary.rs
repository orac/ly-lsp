//! The set of commands ly-lsp recognises.
//!
//! LilyPond's built-in commands are parsed from its `lilypond-words` file; on
//! top of that sit our own special cases — commands the words file omits, and
//! the CamelCase context-reference convention. This module is also the intended
//! home for richer per-command knowledge later (argument checking and
//! refactorings for the likes of `\repeat`); for now it only answers whether a
//! command is recognised at all.

use std::collections::HashSet;
use std::path::Path;

/// Commands that are valid but absent from `lilypond-words`, so we supply them
/// ourselves:
///
/// - `with` only ever appears glued to a context (`\new Staff \with { … }`),
///   so the words file doesn't list it as a standalone command.
/// - `discant` is defined in Scheme by `#(use-modules (lily accreg))`. We don't
///   parse that definition, so we name the command explicitly rather than chase
///   it through the module.
const EXTRA_COMMANDS: &[&str] = &["with", "discant"];

/// The commands ly-lsp knows about: LilyPond's built-ins plus our extras.
///
/// Context references (`\Staff`, `\PianoStaff`, and user-defined contexts) are
/// recognised by their CamelCase initial rather than stored here: the words
/// file lists context names without a backslash, indistinguishable from the
/// grob and engraver names we *don't* want to accept as commands.
#[derive(Debug, Default)]
pub struct Vocabulary {
    /// Command names, without their leading backslash.
    commands: HashSet<String>,
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
        let mut commands = parse_words(text);
        commands.extend(EXTRA_COMMANDS.iter().map(|s| (*s).to_string()));
        Self { commands }
    }

    /// Whether `\name` is a command we recognise. `name` is the command without
    /// its leading backslash.
    ///
    /// CamelCase names are accepted unconditionally: by LilyPond convention a
    /// `\Foo` with an uppercase initial is a context reference (a built-in like
    /// `\Staff` or a user-defined context), which the words file doesn't carry
    /// as a command. The price is that a mistyped context name goes unflagged.
    pub fn is_known(&self, name: &str) -> bool {
        is_context_reference(name) || self.commands.contains(name)
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
        assert!(vocab.is_known("with"));
        assert!(vocab.is_known("discant"));
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
}
