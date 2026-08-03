//! `\tempo`, in each of its three written forms.

use crate::command::curated;

use super::static_command::{StaticCommand, static_command};
use super::{Arg, ArgKind, ArgReader, Candidate, Command, MusicContext, Param};

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
/// so `music_context` is left at the (delegated) `StaticCommand` default,
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

    fn completions(&self, index: usize) -> &[Candidate] {
        self.base.completions(index)
    }
}

/// Builds the `\tempo` entry for [`BUILTIN`](super::BUILTIN).
pub(super) fn command() -> TempoCommand {
    TempoCommand {
        base: static_command(
            "tempo",
            TEMPO_PARAMS,
            MusicContext::Inherit,
            curated("Sets the tempo. `\\tempo \"Allegro\" 4=120` Either argument may be omitted."),
            &[],
        ),
    }
}
