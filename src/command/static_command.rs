//! The plain-signature layer every hand-written command starts from.

use super::{Candidate, Command, CommandCall, Documentation, MusicContext, Param};

/// A command whose only job is to consume a fixed signature and, if it
/// establishes one, set a fixed [`MusicContext`] for its body — every
/// hand-written command except `\relative`, `\fixed` (whose context depends on
/// the pitch they themselves parse) and `\tempo` (whose shape isn't a plain
/// parameter list). One instance per row of [`STATIC_ROWS`](super::STATIC_ROWS)'s
/// table, plus one `base` inside each bespoke wrapper in this module's sibling
/// files; `\chordmode` and its alias `\chords` are two separate instances
/// sharing the same `params`/`context` but each reporting its own `name`.
pub(super) struct StaticCommand {
    pub(super) name: &'static str,
    pub(super) params: &'static [Param],
    /// `MusicContext::Inherit` for a command that neither establishes nor
    /// blocks a music context (`\repeat`, `\set`, …, and every command with no
    /// [`Music`](super::ArgKind::Music) parameter at all); a fixed variant for one
    /// that does (`\chordmode` always reads its body as `Chord`, regardless of
    /// what it was itself found in).
    pub(super) context: MusicContext,
    /// Curated hover prose, where we have any worth showing. `None` for the
    /// majority of rows in [`STATIC_ROWS`](super::STATIC_ROWS)'s table, which say
    /// nothing beyond their signature — padding every mode-switch and
    /// header-block command with a restatement of its own name would cost
    /// more reading than it saves.
    pub(super) documentation: Option<Documentation>,
    /// Completions for each parameter position, indexed the same way
    /// [`Command::completions`] is asked: `completions[i]` answers index `i`.
    /// Shorter than `params`, or empty, for the (overwhelming majority of)
    /// commands with no closed-value argument worth offering.
    pub(super) completions: &'static [&'static [Candidate]],
}

impl Command for StaticCommand {
    fn name(&self) -> &str {
        self.name
    }

    fn signature(&self) -> &[Param] {
        self.params
    }

    fn music_context(&self, _call: &CommandCall, ambient: MusicContext) -> MusicContext {
        match self.context {
            MusicContext::Inherit => ambient,
            fixed => fixed,
        }
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.documentation.as_ref()
    }

    fn completions(&self, index: usize) -> &[Candidate] {
        self.completions.get(index).copied().unwrap_or(&[])
    }
}

/// Builds an owned [`StaticCommand`], the shared constructor every row of
/// [`STATIC_ROWS`](super::STATIC_ROWS) uses, and every bespoke wrapper in this
/// module's sibling files uses for its `base`.
pub(super) fn static_command(
    name: &'static str,
    params: &'static [Param],
    context: MusicContext,
    documentation: Option<Documentation>,
    completions: &'static [&'static [Candidate]],
) -> StaticCommand {
    StaticCommand {
        name,
        params,
        context,
        documentation,
        completions,
    }
}

/// Wraps hand-written Markdown as [`Documentation`] with
/// [`DocSource::Curated`](super::DocSource::Curated) — every documentation
/// string under `src/command/` is our own wording, never LilyPond's, so the
/// source is always the same here.
pub(super) fn curated(markdown: &str) -> Option<Documentation> {
    Some(Documentation {
        markdown: markdown.to_string(),
        source: super::DocSource::Curated,
    })
}
