//! `\language "…"` and `\include "….ly"`, the two ways a file changes the note names its pitches are spelled in.
//!
//! One impl serves both because they do the same thing with the same argument: LilyPond's `ly/english.ly` and its dozen siblings are one-line shims that say `\language "english"`, so a file that includes one has selected a language exactly as if it had said so itself. The only difference is the `.ly` suffix an include's argument carries and a `\language`'s doesn't, and that is a `strip_suffix` rather than a second impl.
//!
//! This is the only place in the server that knows the `\language` command exists. Both the language a call selects and the languages worth offering at its argument come from [`Scope::note_names`] — the installation's own `define-note-names.scm` — so the answer follows whatever LilyPond the workspace is running, and neither the note analyser nor a hand-written candidate table has to hold a second opinion.

use super::static_command::{StaticCommand, curated, static_command};
use super::{
    Arg, Candidate, Command, CommandCall, CompletionContext, Documentation, MusicContext,
    NoteEntry, Param,
};
use crate::note_names::Language;
use crate::vocabulary::Scope;

/// `\language "english"` and `\include "english.ly"`. Wraps a
/// [`StaticCommand`] for its signature and documentation, and overrides the
/// two methods that need to reach the installation's note names:
/// [`music_context`](Command::music_context), which switches the language,
/// and [`completions`](Command::completions), which offers the names it can
/// be switched to.
pub(super) struct LanguageCommand {
    base: StaticCommand,
    /// Whether the argument is *named* as a language, and so worth completing
    /// with language names. True for `\language`, false for `\include`, whose
    /// argument is a filename that only sometimes happens to be a language
    /// shim — offering thirteen note-name languages at every `\include` would
    /// bury the far commoner case of including one's own file.
    offers_language_names: bool,
}

impl Command for LanguageCommand {
    fn name(&self) -> &str {
        self.base.name()
    }

    fn signature(&self) -> &[Param] {
        self.base.signature()
    }

    /// `ambient` with the language this call selects, or `ambient` unchanged
    /// where the call names no language the installation knows — an
    /// ordinary `\include "notes.ly"`, a `\language` still being typed, or a
    /// misspelled one.
    ///
    /// The entry mode is inherited: selecting note names changes how a symbol
    /// is *spelled*, not how its octave is read.
    fn music_context(
        &self,
        call: &CommandCall,
        ambient: MusicContext,
        scope: &Scope,
    ) -> MusicContext {
        match selected(call, scope) {
            Some(language) => ambient.with_language(language),
            None => ambient,
        }
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.base.documentation()
    }

    fn completions(&self, index: usize, ctx: &CompletionContext) -> Vec<Candidate> {
        if !self.offers_language_names || index != 0 {
            return Vec::new();
        }
        ctx.scope
            .note_names()
            .accepted_names()
            .into_iter()
            .map(|(name, language)| Candidate {
                label: name.to_string().into(),
                documentation: format!("Note names: {}.", language.naturals().join(", ")).into(),
            })
            .collect()
    }
}

/// The language `call`'s first string argument selects, if `scope`'s note
/// names know one by that name. The `.ly` of an include's filename is
/// stripped first, so `\include "english.ly"` and `\language "english"`
/// arrive at the same table; a name with a directory in front of it will
/// simply not match, which is right — a file of one's own called `english.ly`
/// is not LilyPond's.
fn selected(call: &CommandCall, scope: &Scope) -> Option<Language> {
    let Some(Arg::String { text, .. }) = call.args.first() else {
        return None;
    };
    scope
        .note_names()
        .language(text.strip_suffix(".ly").unwrap_or(text))
}

/// Builds the `\language` entry for [`CURATED`](super::CURATED). Curated
/// rather than reserved: `\language` is an ordinary music function in
/// LilyPond's own `ly/`, and a file that binds the name itself means its own.
pub(super) fn command(params: &'static [Param]) -> LanguageCommand {
    LanguageCommand {
        base: static_command(
            "language",
            params,
            NoteEntry::Inherit,
            curated(LANGUAGE_DOC),
            &[],
        ),
        offers_language_names: true,
    }
}

/// Builds the `\include` entry for [`RESERVED`](super::RESERVED) — the same
/// impl, because an `\include` of one of LilyPond's language shims selects a
/// language just as `\language` does, and offering no candidates because a
/// filename is not a closed set.
pub(super) fn include(params: &'static [Param]) -> LanguageCommand {
    LanguageCommand {
        base: static_command("include", params, NoteEntry::Inherit, None, &[]),
        offers_language_names: false,
    }
}

const LANGUAGE_DOC: &str = "Selects the note names pitches are written in for the rest of the \
     file — `\\language \"english\"` for `cs`/`ef` rather than the default Dutch `cis`/`ees`. \
     Including the matching file (`\\include \"english.ly\"`) does the same thing.";
