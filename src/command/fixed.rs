//! `\fixed reference { … }`.

use super::static_command::{StaticCommand, static_command};
use super::{
    Arg, Candidate, Command, CommandCall, CompletionContext, MusicContext, NoteEntry, Param,
    REFERENCE_PITCH_PARAMS,
};
use crate::vocabulary::Scope;

/// `\fixed reference { … }`. Like `\relative`, the body's context depends on
/// the argument it parses itself: the reference pitch's octave becomes the
/// [`NoteEntry::Fixed`] offset. Absent a reference (a half-typed `\fixed
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

    fn music_context(
        &self,
        call: &CommandCall,
        ambient: MusicContext,
        _scope: &Scope,
    ) -> MusicContext {
        let offset = call
            .args
            .iter()
            .find_map(|arg| match arg {
                Arg::Pitch { pitch, .. } => Some(pitch.octave),
                _ => None,
            })
            .unwrap_or(-1);
        ambient.with_entry(NoteEntry::Fixed(offset))
    }

    fn documentation(&self) -> Option<&super::Documentation> {
        self.base.documentation()
    }

    fn completions(&self, index: usize, ctx: &CompletionContext) -> Vec<Candidate> {
        self.base.completions(index, ctx)
    }
}

/// Builds the `\fixed` entry for [`CURATED`](super::CURATED).
pub(super) fn command() -> FixedCommand {
    FixedCommand {
        base: static_command(
            "fixed",
            REFERENCE_PITCH_PARAMS,
            None,
            None,
            Some(super::Documentation {
                markdown:
                    "Reads `music` with a fixed reference octave: an unmarked note sits in the \
                 same octave as `reference`, and octave marks (`'`, `,`) shift from there."
                        .to_string(),
            }),
            &[],
        ),
    }
}
