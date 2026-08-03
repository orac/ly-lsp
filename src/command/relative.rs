//! `\relative [pitch] { … }`.

use super::static_command::{StaticCommand, curated, static_command};
use super::{Arg, Candidate, Command, CommandCall, MusicContext, Param, REFERENCE_PITCH_PARAMS};
use crate::notes::Pitch;

/// LilyPond's default `\relative` reference when none is written: middle C.
const DEFAULT_RELATIVE_REFERENCE: Pitch = Pitch {
    note_name: 0,
    octave: 0,
    alteration: 0,
};

/// `\relative [pitch] { … }`. The reference pitch is optional in the grammar
/// (`\relative { … }` defaults to middle C, matching LilyPond) and, when
/// present, decides the [`MusicContext::Relative`] the body reads in — the one
/// piece of behaviour [`StaticCommand`] can't express, since it needs to read
/// an argument it just parsed rather than answer with a fixed context. Wraps a
/// `StaticCommand` for its `name`/`signature`/`documentation`/`completions`
/// (whose `context` field goes unused, since `music_context` is overridden
/// below rather than delegated) and overrides only that one method.
pub(super) struct RelativeCommand {
    base: StaticCommand,
}

impl Command for RelativeCommand {
    fn name(&self) -> &str {
        self.base.name()
    }

    fn signature(&self) -> &[Param] {
        self.base.signature()
    }

    fn music_context(&self, call: &CommandCall, _ambient: MusicContext) -> MusicContext {
        let reference = call
            .args
            .iter()
            .find_map(|arg| match arg {
                Arg::Pitch { pitch, .. } => Some(*pitch),
                _ => None,
            })
            .unwrap_or(DEFAULT_RELATIVE_REFERENCE);
        MusicContext::Relative(reference)
    }

    fn documentation(&self) -> Option<&super::Documentation> {
        self.base.documentation()
    }

    fn completions(&self, index: usize) -> &[Candidate] {
        self.base.completions(index)
    }
}

/// Builds the `\relative` entry for [`BUILTIN`](super::BUILTIN).
pub(super) fn command() -> RelativeCommand {
    RelativeCommand {
        base: static_command(
            "relative",
            REFERENCE_PITCH_PARAMS,
            MusicContext::Inherit,
            curated(
                "Reads `music` with octaves written relative to the previous note: each \
                 note is placed in the octave closest to the one before it, starting from \
                 `reference` (middle C if omitted).",
            ),
            &[],
        ),
    }
}
