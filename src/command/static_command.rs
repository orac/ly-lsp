//! The plain-signature layer every hand-written command starts from.

use super::{
    Candidate, Command, CommandCall, CompletionContext, Documentation, MusicContext, NoteEntry,
    Param, Region,
};
use crate::vocabulary::Scope;

/// A command whose only job is to consume a fixed signature and, if it
/// establishes one, set a fixed [`MusicContext`] for its body — every
/// hand-written command except `\relative`, `\fixed` (whose context depends on
/// the pitch they themselves parse) and `\tempo` (whose shape isn't a plain
/// parameter list). One instance per row of the hand-written tables'
/// table, plus one `base` inside each bespoke wrapper in this module's sibling
/// files; `\chordmode` and its alias `\chords` are two separate instances
/// sharing the same `params`/`entry`/`region` but each reporting its own `name`.
///
/// Two of a [`MusicContext`]'s three pieces can be set from a row, and never
/// the third: a row is a `'static` table and a
/// [`Language`](crate::note_names::Language) is read from an installation at
/// runtime. In any case no plain row switches languages — `\language` is the
/// one command that does, and it has an impl of its own in
/// [`language`](super::language).
pub(super) struct StaticCommand {
    pub(super) name: &'static str,
    pub(super) params: &'static [Param],
    /// The octave entry this command establishes for its body, if it
    /// establishes one. `Some` for `\notemode` alone among the plain rows;
    /// `None` for everything else, which inherits whatever the call site was
    /// read in.
    ///
    /// Setting this also puts the body in [`Region::NoteMusic`] — see
    /// [`MusicContext::with_entry`] — since saying how octaves are written
    /// is only meaningful where the symbols are notes.
    pub(super) entry: Option<NoteEntry>,
    /// The region this command establishes for its body, if it establishes
    /// one: `\chordmode` always reads its body as chord music and `\header`
    /// always as non-note, regardless of what either was itself found in.
    /// `None` for the majority, which neither establish nor block a region of
    /// their own (`\repeat`, `\set`, …, and every command with no
    /// [`Music`](super::ArgKind::Music) parameter at all).
    ///
    /// Applied after [`entry`](Self::entry), so a row that somehow set both
    /// would get the region it asked for rather than the `NoteMusic` the
    /// entry implies. No row does.
    pub(super) region: Option<Region>,
    /// Curated hover prose, where we have any worth showing. `None` for the
    /// majority of rows in the hand-written tables, which say
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

    fn music_context(
        &self,
        _call: &CommandCall,
        ambient: MusicContext,
        _scope: &Scope,
    ) -> MusicContext {
        // Each piece the row has an opinion about replaces the ambient one;
        // everything else is inherited, which for the overwhelming majority
        // of rows means all of it.
        let established = match self.entry {
            Some(entry) => ambient.with_entry(entry),
            None => ambient,
        };
        match self.region {
            Some(region) => established.with_region(region),
            None => established,
        }
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.documentation.as_ref()
    }

    fn completions(&self, index: usize, _ctx: &CompletionContext) -> Vec<Candidate> {
        self.completions
            .get(index)
            .map_or_else(Vec::new, |candidates| candidates.to_vec())
    }
}

/// Builds an owned [`StaticCommand`], the shared constructor every row of
/// the hand-written tables use, and every bespoke wrapper in this
/// module's sibling files uses for its `base`.
pub(super) fn static_command(
    name: &'static str,
    params: &'static [Param],
    entry: Option<NoteEntry>,
    region: Option<Region>,
    documentation: Option<Documentation>,
    completions: &'static [&'static [Candidate]],
) -> StaticCommand {
    StaticCommand {
        name,
        params,
        entry,
        region,
        documentation,
        completions,
    }
}

/// Wraps hand-written Markdown as [`Documentation`] — every documentation
/// string under `src/command/` is our own wording.
pub(super) fn curated(markdown: &str) -> Option<Documentation> {
    Some(Documentation {
        markdown: markdown.to_string(),
    })
}
