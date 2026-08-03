//! `\fixed reference { … }`.

use super::static_command::{StaticCommand, curated, static_command};
use super::{
    Arg, Candidate, Command, CommandCall, MusicContext, Param, REFERENCE_PITCH_PARAMS, clamp_octave,
};

/// `\fixed reference { … }`. Like `\relative`, the body's context depends on
/// the argument it parses itself: the reference pitch's octave becomes the
/// [`MusicContext::Fixed`] offset. Absent a reference (a half-typed `\fixed
/// {`), falls back to `-1`, the offset an unmarked absolute `c` resolves to —
/// so an incomplete `\fixed` reads its body as plain absolute entry until a
/// reference is written. Wraps a `StaticCommand` the same way
/// [`RelativeCommand`](super::relative) does.
pub(super) struct FixedCommand {
    base: StaticCommand,
}

impl Command for FixedCommand {
    fn name(&self) -> &str {
        self.base.name()
    }

    fn signature(&self) -> &[Param] {
        self.base.signature()
    }

    fn music_context(&self, call: &CommandCall, _ambient: MusicContext) -> MusicContext {
        let offset = call
            .args
            .iter()
            .find_map(|arg| match arg {
                Arg::Pitch { pitch, .. } => Some(pitch.octave),
                _ => None,
            })
            .unwrap_or(-1);
        MusicContext::Fixed(clamp_octave(offset))
    }

    fn documentation(&self) -> Option<&super::Documentation> {
        self.base.documentation()
    }

    fn completions(&self, index: usize) -> &[Candidate] {
        self.base.completions(index)
    }
}

/// Builds the `\fixed` entry for [`BUILTIN`](super::BUILTIN).
pub(super) fn command() -> FixedCommand {
    FixedCommand {
        base: static_command(
            "fixed",
            REFERENCE_PITCH_PARAMS,
            MusicContext::Inherit,
            curated(
                "Reads `music` with a fixed reference octave: an unmarked note sits in the \
                 same octave as `reference`, and octave marks (`'`, `,`) shift from there.",
            ),
            &[],
        ),
    }
}
