//! `\tempo`, in each of its three written forms.

use super::static_command::{StaticCommand, static_command};
use super::{
    Arg, ArgKind, ArgReader, Candidate, Command, CommandCall, CompletionContext, MusicContext,
    Param,
};
use crate::vocabulary::Scope;

static TEMPO_PARAMS: &[Param] = &[
    Param::optional("text", ArgKind::String),
    Param::optional("duration", ArgKind::Count),
    Param::optional("value", ArgKind::Count),
];

/// `\tempo`, in each of its three written forms: `"text"`, `duration =
/// metronome-number`, or both together. Not expressible as a plain parameter
/// list because of the literal `=` between the duration and the number, so
/// this overrides [`parse_args`](Command::parse_args) instead of relying on
/// [`default_parse`](super::default_parse); [`signature`](Command::signature)
/// still lists the three pieces for signature help and arity checks even
/// though the default parser never walks it. `\tempo` has no music argument,
/// so [`music_context`](Command::music_context) just forwards to the base's
/// `Inherit`.
pub(super) struct TempoCommand {
    base: StaticCommand,
}

impl Command for TempoCommand {
    fn name(&self) -> &str {
        self.base.name()
    }

    fn signature(&self) -> &[Param] {
        self.base.signature()
    }

    fn parse_args(&self, args: &mut ArgReader) -> Vec<Arg> {
        let mut out = Vec::new();
        if let Some(text) = args.take(&ArgKind::String) {
            out.push(text);
        }
        if let Some(duration) = args.take(&ArgKind::Count) {
            out.push(duration);
            if args.take_punct("=")
                && let Some(value) = args.take(&ArgKind::Count)
            {
                out.push(value);
            }
        }
        out
    }

    fn documentation(&self) -> Option<&super::Documentation> {
        self.base.documentation()
    }

    // `\tempo`'s base is `MusicContext::Inherit`, for which the trait's
    // default (return `ambient` unchanged) and this forward already agree —
    // but relying on that default is what let `\lyricsto` regress silently
    // when its own base turned out to be `NonNote` (`super::lyricsto`).
    // Forwarded explicitly so this wrapper can't drift the same way if
    // `\tempo`'s context ever stops being `Inherit`.
    fn music_context(
        &self,
        call: &CommandCall,
        ambient: MusicContext,
        scope: &Scope,
    ) -> MusicContext {
        self.base.music_context(call, ambient, scope)
    }

    fn completions(&self, index: usize, ctx: &CompletionContext) -> Vec<Candidate> {
        self.base.completions(index, ctx)
    }
}

/// Builds the `\tempo` entry for [`RESERVED`](super::RESERVED).
pub(super) fn command() -> TempoCommand {
    TempoCommand {
        base: static_command(
            "tempo",
            TEMPO_PARAMS,
            MusicContext::Inherit,
            Some(super::Documentation {
                markdown:
                    "Sets the tempo. `\\tempo \"Allegro\" 4=120` Either argument may be omitted."
                        .to_string(),
            }),
            &[],
        ),
    }
}
