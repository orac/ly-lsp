//! `\version "2.24.3"`.

use super::static_command::{StaticCommand, static_command};
use super::{
    ArgKind, Candidate, Command, CommandCall, CompletionContext, Documentation, MusicContext,
    NoteEntry, Param, static_command::curated,
};
use crate::vocabulary::Scope;

static VERSION_PARAMS: &[Param] = &[Param::required("version", ArgKind::String)];

const VERSION_DOC: &str = "Declares the LilyPond version this file is written for. `convert-ly` \
     reads it to know which conversions to apply, so it is the first line of every score.";

/// `\version "2.24.3"`. A [`StaticCommand`] shape apart from its one argument,
/// which is the one thing in the language that only the running machine knows:
/// the version of the LilyPond that is actually installed. So the candidate is
/// computed from the [`CompletionContext`] rather than written down here, and a
/// workspace with no installation behind it simply offers nothing rather than
/// guessing a number.
pub(super) struct VersionCommand {
    base: StaticCommand,
}

impl Command for VersionCommand {
    fn name(&self) -> &str {
        self.base.name()
    }

    fn signature(&self) -> &[Param] {
        self.base.signature()
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.base.documentation()
    }

    // `\version`'s base is `NoteEntry::Inherit`, for which the trait's
    // default (return `ambient` unchanged) and this forward already agree —
    // but relying on that default is what let `\lyricsto` regress silently
    // when its own base turned out to be `NonNote` (`super::lyricsto`).
    // Forwarded explicitly so this wrapper can't drift the same way if
    // `\version`'s context ever stops being `Inherit`.
    fn music_context(
        &self,
        call: &CommandCall,
        ambient: MusicContext,
        scope: &Scope,
    ) -> MusicContext {
        self.base.music_context(call, ambient, scope)
    }

    fn completions(&self, index: usize, ctx: &CompletionContext) -> Vec<Candidate> {
        match (index, ctx.lilypond_version) {
            (0, Some(version)) => vec![Candidate {
                label: version.to_string().into(),
                documentation: "The version of LilyPond you have installed.".into(),
            }],
            _ => Vec::new(),
        }
    }
}

/// Builds the `\version` entry for [`RESERVED`](super::RESERVED).
pub(super) fn command() -> VersionCommand {
    VersionCommand {
        base: static_command(
            "version",
            VERSION_PARAMS,
            NoteEntry::Inherit,
            curated(VERSION_DOC),
            &[],
        ),
    }
}
