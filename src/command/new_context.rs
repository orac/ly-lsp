//! `\new type [= "name"] [\with { … }] music` and `\context type [= "name"]
//! [\with { … }] music`.
//!
//! Both are reserved words — LilyPond's grammar recognises them before any
//! name lookup happens, and folds the keyword together with the context type
//! into one `named_context` node ([`parse`](super::parse) unwraps that shape
//! before either command ever sees an [`ArgReader`]). They differ from every
//! other row in [`RESERVED_ROWS`](super::RESERVED_ROWS) in one way that stops
//! them being a plain [`Row`](super::Row): the [`MusicContext`] their body
//! reads in isn't fixed, the way `\chordmode`'s always is, but depends on
//! *which* context type the call names — `\new Lyrics` reads its body as
//! [`NoteEntry::NonNote`], `\new Staff` doesn't. So both get this one
//! bespoke impl, told apart only by their `name` and their curated doc
//! string ([`NEW_DOC`](super::NEW_DOC), [`CONTEXT_DOC`](super::CONTEXT_DOC)):
//! `\context`'s "reuse the context if one of this type and name already
//! exists, rather than always creating a fresh one" semantics aren't
//! observed by anything built so far, so there is nothing yet to tell the
//! two apart beyond their names and their prose.

use std::borrow::Cow;

use super::static_command::{StaticCommand, curated, static_command};
use super::{
    Arg, ArgKind, ArgReader, Candidate, Command, CommandCall, CompletionContext, Documentation,
    MusicContext, NoteEntry, Param, context_instance_candidates, context_type_candidates,
};
use crate::line_struct::Span;
use crate::vocabulary::Scope;

/// The `= "name"` clause inside [`NEW_CONTEXT_PARAMS`], named separately
/// because a `static`'s initialiser can't hold an inline temporary slice of
/// non-`Copy` [`Param`]s.
static NEW_CONTEXT_ASSIGNMENT_PARAMS: &[Param] = &[
    Param::required("=", ArgKind::Literal("=")),
    Param::required("name", ArgKind::ContextName),
];

/// `type`, the optional `= name` clause, the optional `\with { … }` block,
/// and `music` — the four pieces [`NewContextCommand::parse_args`] reads by
/// hand, listed here only so signature help and arity checks have parameter
/// names to show. `= name` is an [`ArgKind::Group`] rather than two
/// independent optional pieces — a bare `\new Staff name` (no `=`) must not
/// misread `name` as the following music — but
/// [`default_parse`](super::default_parse) still never walks this on its
/// own: the `\with` block needs its own keyword-and-block handling, and
/// `named_context`'s flattening ([`super::parse`]) means this reader starts
/// mid-stream rather than at a plain `escaped_word`.
static NEW_CONTEXT_PARAMS: &[Param] = &[
    Param::required("type", ArgKind::ContextType),
    Param::optional("= name", ArgKind::Group(NEW_CONTEXT_ASSIGNMENT_PARAMS)),
    Param::optional("with", ArgKind::Unknown(Cow::Borrowed("with block"))),
    Param::required("music", ArgKind::Music),
];

/// The context types whose body is read as [`NoteEntry::NonNote`] rather
/// than ordinary note music — lyrics, chord names, drum staves and the like,
/// where a bare symbol means something other than a pitch.
///
/// Hand-maintained: LilyPond's own initialisation files don't mark a context
/// type as "reads its body as music" or not anywhere this reader can find,
/// so there is no way to derive this list from the install the way
/// [`ContextType`](crate::context::ContextType)'s other fields are derived.
/// It names *roots* rather than every possible non-note context, because
/// [`is_non_note`] also follows a type's declared aliases: a user's own
/// `\context { \name MyLyrics \alias Lyrics }` is non-note by inheriting
/// from a root here, without needing its own entry.
const NON_NOTE_CONTEXT_ROOTS: &[&str] = &[
    "Lyrics",
    "NullVoice",
    "ChordNames",
    "FretBoards",
    "FiguredBass",
    "Dynamics",
    "DrumStaff",
    "DrumVoice",
];

/// Whether `type_name` — a context type as written in a `\new`/`\context`
/// call — reads its body as non-note music: either it names one of
/// [`NON_NOTE_CONTEXT_ROOTS`] directly, or `scope` knows a declaration for
/// it that carries one of those roots as an `\alias`.
///
/// Falls back to checking `type_name` alone, with no alias to consult, when
/// `scope` has no [`ContextType`](crate::context::ContextType) on record for
/// it at all — unlike [`Scope::is_known`]'s fallback, which only fires when
/// the whole install failed to load, this one fires per name, an ordinary
/// typo included. That is deliberately looser, and costs nothing: every name
/// in [`NON_NOTE_CONTEXT_ROOTS`] is a builtin any successfully loaded install
/// already knows, so with a real install behind `scope` this branch is only
/// ever reached by a name that isn't a root either way — the fallback and
/// the alias check agree on `false` for it. Gating this on `Scope`'s own
/// install-failed flag too, to mirror `is_known` exactly, would change no
/// observable behaviour for that reason, so it isn't done.
fn is_non_note(type_name: &str, scope: &Scope) -> bool {
    match scope.get_context_type(type_name) {
        Some(known) => {
            NON_NOTE_CONTEXT_ROOTS.contains(&known.value.name.as_str())
                || known
                    .value
                    .aliases
                    .iter()
                    .any(|alias| NON_NOTE_CONTEXT_ROOTS.contains(&alias.as_str()))
        }
        None => NON_NOTE_CONTEXT_ROOTS.contains(&type_name),
    }
}

/// `\new`/`\context`. Wraps a [`StaticCommand`] for its
/// `name`/`signature`/`documentation`/`completions`, overriding
/// [`parse_args`](Command::parse_args) (the `= "name"` and `\with { … }`
/// shapes no plain [`Param`] list expresses) and
/// [`music_context`](Command::music_context) (a fixed [`MusicContext`]
/// can't answer for every context type at once).
pub(super) struct NewContextCommand {
    base: StaticCommand,
}

impl Command for NewContextCommand {
    fn name(&self) -> &str {
        self.base.name()
    }

    fn signature(&self) -> &[Param] {
        self.base.signature()
    }

    fn parse_args(&self, args: &mut ArgReader) -> Vec<Arg> {
        let mut out = Vec::new();
        let Some(context_type) = args.take(&ArgKind::ContextType) else {
            return out;
        };
        out.push(context_type);

        if let Some(assignment) = args.take(&ArgKind::Group(NEW_CONTEXT_ASSIGNMENT_PARAMS)) {
            out.push(assignment);
        }

        // `\with { … }`: consumed as one `Arg::Unknown` spanning the keyword
        // and its block together, deliberately *not* `Arg::Music` — the note
        // analyser walks every `Arg::Music` in a call as part of its body,
        // and a `\with` block's contents are property settings (`fontSize =
        // #-2`), not music; reading them as such would misread a property
        // name as an invalid note. It doesn't go through the ordinary
        // `ArgKind::Unknown` path in `consume_arg` either, which *declines*
        // to consume a music-shaped node like this block on purpose (see the
        // comment there) — precisely the shape this needs to consume, so
        // `ArgReader::skip_one` steps over both nodes unconditionally
        // instead.
        if args.peek_text() == Some("\\with")
            && let Some(with_span) = args.skip_one()
        {
            let end = args.skip_one().map_or(with_span.end, |block| block.end);
            out.push(Arg::Unknown {
                span: Span::new(with_span.start, end),
            });
        }

        if let Some(music) = args.take(&ArgKind::Music) {
            out.push(music);
        }
        out
    }

    fn music_context(
        &self,
        call: &CommandCall,
        ambient: MusicContext,
        scope: &Scope,
    ) -> MusicContext {
        let type_name = call.args.iter().find_map(|arg| match arg {
            Arg::ContextType { name, .. } => Some(name.as_str()),
            _ => None,
        });
        match type_name {
            Some(type_name) if is_non_note(type_name, scope) => {
                ambient.with_entry(NoteEntry::NonNote)
            }
            _ => ambient,
        }
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.base.documentation()
    }

    fn completions(&self, index: usize, ctx: &CompletionContext) -> Vec<Candidate> {
        match index {
            0 => context_type_candidates(ctx.scope),
            // 1 is the whole `= name` Group — its only completable piece is
            // `name`, so the cursor anywhere in the clause (including on the
            // `=` itself, which offers nothing anyway) gets instance names.
            1 => context_instance_candidates(ctx.scope),
            _ => self.base.completions(index, ctx),
        }
    }
}

/// Builds one `\new`/`\context` entry for [`RESERVED`](super::RESERVED).
/// Called twice, with `("new", NEW_DOC)` and `("context", CONTEXT_DOC)`,
/// since the two share everything but their name and their prose.
pub(super) fn command(name: &'static str, doc: &'static str) -> NewContextCommand {
    NewContextCommand {
        base: static_command(
            name,
            NEW_CONTEXT_PARAMS,
            NoteEntry::Inherit,
            curated(doc),
            &[],
        ),
    }
}
