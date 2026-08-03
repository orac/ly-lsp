//! `\repeat kind count music`.

use tower_lsp::lsp_types::Diagnostic;

use super::static_command::{StaticCommand, curated, static_command};
use super::{Arg, ArgKind, Candidate, CheckContext, Command, CommandCall, MusicContext, Param};

static REPEAT_PARAMS: &[Param] = &[
    Param::required("kind", ArgKind::BareWord),
    Param::required("count", ArgKind::Count),
    Param::required("music", ArgKind::Music),
];

/// `\repeat`'s four kinds, offered as completions at its `kind` parameter
/// (index 0).
static REPEAT_KIND_CANDIDATES: &[Candidate] = &[
    Candidate {
        label: "volta",
        documentation: "Double-dot repeat barlines, optionally with alternate endings (first time bar &c.). Usually full bars, and vertically aligned across a full system.",
    },
    Candidate {
        label: "unfold",
        documentation: "Writes the repeated music out in full, `count` times. Can be used on an individual voice or staff.",
    },
    Candidate {
        label: "percent",
        documentation: "Slash or percent signs for repeating a single beat or bar within the structure of bars. Common for percussion. Can be used on one staff of a system.",
    },
    Candidate {
        label: "tremolo",
        documentation: "A tremolo repeat, beamed between the repeated notes.",
    },
    Candidate {
        label: "segno",
        documentation: "For D.C./D.S. al coda/fine repeats, often with volta repeats nested inside. Vertically aligned across a full system.",
    },
];
static REPEAT_COMPLETIONS: &[&[Candidate]] = &[REPEAT_KIND_CANDIDATES];

/// `\repeat`'s five recognised kinds, checked against by [`RepeatCommand::check`].
const REPEAT_KINDS: [&str; 5] = ["volta", "unfold", "percent", "tremolo", "segno"];

/// `\repeat kind count music`. A [`StaticCommand`] shape apart from one
/// thing: [`check`](Command::check) flags a `kind` that isn't one of
/// LilyPond's five recognised repeat types, and a `count` of `0`, which
/// repeats its music zero times — both rejected by LilyPond itself, the
/// first at parse time (the `kind` string is matched against a fixed set),
/// the second because the repeat iterator requires a positive count. A
/// negative count can't be written here in the first place: `count`'s
/// [`ArgKind::Count`] only ever consumes an `unsigned_integer`. Wraps a
/// `StaticCommand` for its `name`/`signature`/`documentation`/`completions`,
/// overriding only `check`.
pub(super) struct RepeatCommand {
    base: StaticCommand,
}

impl Command for RepeatCommand {
    fn name(&self) -> &str {
        self.base.name()
    }

    fn signature(&self) -> &[Param] {
        self.base.signature()
    }

    fn documentation(&self) -> Option<&super::Documentation> {
        self.base.documentation()
    }

    fn completions(&self, index: usize) -> &[Candidate] {
        self.base.completions(index)
    }

    fn check(&self, call: &CommandCall, ctx: &CheckContext) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        if let Some(Arg::BareWord { span, text }) = call.args.first()
            && !REPEAT_KINDS.contains(&text.as_str())
        {
            diagnostics.push(ctx.error(
                *span,
                format!(
                    "`{text}` is not a `\\repeat` kind LilyPond recognises; expected one of \
                     `volta`, `unfold`, `percent`, `tremolo` or `segno`"
                ),
            ));
        }
        if let Some(Arg::Count { span, value: 0 }) = call.args.get(1) {
            diagnostics.push(
                ctx.error(
                    *span,
                    "a `\\repeat` count of 0 repeats its music zero times; use a count of at \
                 least 1"
                        .to_string(),
                ),
            );
        }
        diagnostics
    }
}

/// Builds the `\repeat` entry for [`BUILTIN`](super::BUILTIN).
pub(super) fn command() -> RepeatCommand {
    RepeatCommand {
        base: static_command(
            "repeat",
            REPEAT_PARAMS,
            MusicContext::Inherit,
            curated(
                "Repeats `music` `count` times, using `kind` to say how: `volta` for \
                 numbered alternate endings (with a following `\\alternative` supplying \
                 them), `unfold` to write the repeat out in full, `percent` for a percent \
                 (simile) repeat sign, `tremolo` for a tremolo repeat, or `segno` for a \
                 volta-shaped repeat marked with segno/coda signs and D.S./D.C. markup \
                 instead of numbered brackets.",
            ),
            REPEAT_COMPLETIONS,
        ),
    }
}
