//! Command/argument parsing.
//!
//! LilyPond commands (`\repeat`, `\volta`, `\relative`, `\key`, …) take
//! arguments in command-specific shapes that the tree-sitter grammar leaves as a
//! flat run of sibling nodes — `\repeat volta 2 { … }` is an `escaped_word`, a
//! `symbol`, an `unsigned_integer` and an `expression_block`, all siblings. This
//! module recognises a command from its `escaped_word` and consumes the
//! arguments its signature calls for, producing a structured [`CommandCall`] that
//! the note analyser, the refactorings and (later) a completion provider can
//! share instead of each re-deriving the shape ad hoc.
//!
//! The knowledge of *which* commands exist and what their arguments look like
//! lives behind the [`Command`] trait. The hand-written knowledge is two
//! layers, because it answers two different questions: [`RESERVED`] holds the
//! words LilyPond's own grammar recognises (`\repeat`, `\set`, the mode
//! switches, …), which nothing can rebind; [`CURATED`] holds our better
//! signatures for ordinary music functions LilyPond defines in `ly/`
//! (`\clef`, `\key`, `\relative`, …), which a file *can* shadow. Most of
//! both are plain rows of [`RESERVED_ROWS`] and [`CURATED_ROWS`], built as a
//! [`StaticCommand`](static_command::StaticCommand) — `\tempo` among them,
//! its `duration = value` clause an [`ArgKind::Group`] rather than a reason
//! to be bespoke; the handful with genuinely irregular behaviour —
//! [`relative`], [`fixed`],
//! [`repeat`], [`new_context`] (serving both `\new` and `\context`, whose
//! body's [`MusicContext`] depends on the context type named in the call,
//! not on a fixed row), [`change`] (whose context type sometimes, but not
//! always, arrives wrapped in an `assignment_lhs` node), [`lyricsto`]
//! (whose voice-name parameter, like `new_context`'s and `change`'s context
//! name, is completed by looking the document's [`Scope`] up rather than
//! from a fixed table) and [`language`] (serving both `\language` and
//! `\include`, since one of LilyPond's language files is a `\language`
//! shim, and reading both the language it selects and the names worth
//! offering out of the installation's own note-name data) — each get their
//! own file here, wrapping a
//! `StaticCommand` and overriding the one method that makes them bespoke.
//! The other layer built so far is the user's own files. [`definition`] builds
//! one [`Layer`] per file out of everything it binds — a definition being a
//! command that takes no arguments unless something says otherwise — and the
//! document graph stacks those into a [`Scope`]. [`scheme`] is what supplies
//! the "unless": it reads the `define-music-function`s inside `#( … )` for the
//! arguments they declare. See
//! [`doc/command-parsing.md`](../../doc/command-parsing.md) for the fuller
//! design, including the `install` layer still to come.

mod change;
pub mod definition;
mod fixed;
mod language;
mod lyricsto;
mod new_context;
mod relative;
mod repeat;
pub mod scheme;
mod static_command;
pub mod variable;
mod version;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};
use tree_sitter::Node;

use crate::line_struct::{LineIndex, Span};
use crate::note_names::Language;
use crate::notes::Pitch;
use crate::vocabulary::{Layer, Scope};
use static_command::{curated, static_command};

/// A `\word` the server understands: the arguments it takes, what to say about
/// it on hover, and what to offer inside its arguments.
///
/// There is one impl per *source of knowledge*, not one per command:
/// [`StaticCommand`](static_command::StaticCommand) is a single struct,
/// instantiated once per row of the hand-written tables, for every
/// command whose only job is to consume a fixed signature and (maybe) set a
/// fixed [`MusicContext`] for its body; [`relative::RelativeCommand`],
/// [`fixed::FixedCommand`] and [`tempo::TempoCommand`] are the three whose
/// behaviour genuinely differs. From the user's own files,
/// [`SchemeCommand`](scheme::SchemeCommand) is instantiated once per
/// `define-…-function` and [`Variable`](variable::Variable) once per
/// everything else bound.
///
/// One impl decorates rather than answers: `definition::Definition` wraps any
/// of the above with where its file wrote the name, so
/// [`definition`](Self::definition) and [`redefines`](Self::redefines) are
/// implemented once for every source of knowledge rather than once each.
///
/// Deliberately object-safe: a [`Layer`](crate::vocabulary::Layer)
/// stores `Arc<dyn Command>` and hands them out by name, so no method may be
/// generic or return `Self`. In particular `check` returns a `Vec` rather than
/// `impl Iterator`, because an RPITIT would make the trait dyn-incompatible.
pub trait Command: Send + Sync {
    /// The name as written, without its leading backslash (`repeat`).
    fn name(&self) -> &str;

    /// The parameters this command expects, in source order.
    ///
    /// Impls that override [`parse_args`](Command::parse_args) still return
    /// their parameters here, because signature help and arity diagnostics
    /// read them even when the parsing is irregular. A command we know only by
    /// name — a `lilypond-words` entry with no definition behind it — returns
    /// an empty slice, and its following block is then read as music by the
    /// analyser's main loop, which is what happens today.
    fn signature(&self) -> &[Param];

    /// Consumes this command's arguments from the siblings following its
    /// keyword.
    ///
    /// The default walks [`signature`](Command::signature): a required
    /// parameter that fails to match stops consumption, so a half-typed
    /// `\repeat volta` yields the arguments seen so far rather than nothing; an
    /// optional parameter that fails is skipped and the next parameter tried
    /// against the same node.
    ///
    /// Override only when the shape can't be expressed as a parameter list at
    /// all — not because a piece is irregular in isolation ([`ArgKind::Literal`]
    /// covers a fixed token like `\tempo`'s `=` on its own), but because
    /// whether it's expected depends on what came before: `\tempo`'s `=` and
    /// metronome number only belong once a duration was actually read, and
    /// [`default_parse`]'s param-by-param walk can't express that
    /// conditioning. Do not override merely to reject a bad argument; that
    /// belongs in [`check`](Command::check), so that a wrong-but-parseable
    /// call still produces a structured [`CommandCall`] for the refactorings
    /// to work with.
    fn parse_args(&self, args: &mut ArgReader) -> Vec<Arg> {
        default_parse(self.signature(), args)
    }

    /// How the note analyser should read this call's music arguments — the
    /// mode `\relative` establishes, the chord region `\chordmode` establishes,
    /// the non-note region `\lyricmode` establishes.
    ///
    /// Takes the parsed call because the answer often depends on an argument:
    /// `\relative c'` reads its own reference pitch out of `call` and returns
    /// `ambient.with_entry(NoteEntry::Relative(pitch))`. Takes `ambient`
    /// because every context is built from the one it was reached in — a
    /// command that changes the entry mode inherits the language, and
    /// `\language`, which changes the language, inherits the entry mode. Takes `scope` because deciding whether
    /// a `\new`/`\context` body is note music depends on the named context
    /// type's declaration — its aliases in particular — which lives in the
    /// document's [`Scope`], not in anything the call itself carries. The
    /// default, returning `ambient` unchanged and ignoring `scope`, is right
    /// for the overwhelming majority of commands, which neither establish nor
    /// block a music context of their own.
    fn music_context(
        &self,
        _call: &CommandCall,
        ambient: MusicContext,
        _scope: &Scope,
    ) -> MusicContext {
        ambient
    }

    /// What this command *is*, as a block of Markdown: the headline of its
    /// hover, above whatever [`documentation`](Command::documentation) adds.
    ///
    /// The default is the signature, which is what there is to say about a
    /// command that takes arguments. Override where the shape of the thing
    /// itself is the interesting part: a [`Variable`](variable::Variable) shows
    /// what it is bound to, a zero-argument signature being nothing but the
    /// name the reader is already looking at. `None` where there is neither —
    /// a name we know from a word list and no more — and hover then falls
    /// silent unless the documentation carries it.
    fn synopsis(&self) -> Option<String> {
        let params = self.signature();
        (!params.is_empty()).then(|| code_block(&signature_label(self.name(), params)))
    }

    /// Hover documentation, already rendered to Markdown. `None` for a command
    /// we recognise but can say nothing about — most of the hand-written
    /// rows, which say nothing beyond their signature.
    fn documentation(&self) -> Option<&Documentation> {
        None
    }

    /// Where this command's name is written, within the file whose [`Layer`]
    /// it came from.
    ///
    /// `None` for the hand-written commands, which are defined in this repo
    /// rather than in anyone's score, and so have nothing to navigate to.
    ///
    /// A span, not a range or a URL: a [`Layer`] belongs to one file, and the
    /// caller that asked it for a command already knows which.
    fn definition(&self) -> Option<Span> {
        None
    }

    /// The definition this one replaced, where its file binds the same name
    /// more than once.
    ///
    /// LilyPond takes the last binding of a name, so that is the one the layer
    /// hands out — but the earlier ones are still *written*, and are still
    /// what go-to-definition and document highlights should point at. Keeping
    /// them on a chain rather than as a flat list of spans also leaves the
    /// replaced command itself reachable, which a warning or code lens about a
    /// redefinition would want. See [`definition_spans`].
    fn redefines(&self) -> Option<&Arc<dyn Command>> {
        None
    }

    /// The values worth completing at parameter `index`. Empty when the
    /// parameter is open-ended, which is most parameters of most commands.
    ///
    /// Owned, and given a [`CompletionContext`], so that a command can
    /// *compute* its candidates rather than only point at a table:
    /// [`version`] offers the number of the install the workspace was loaded
    /// from, which nothing written here could know. The price is a clone of
    /// the handful of candidates a table holds, paid only when a completion
    /// list is actually asked for.
    fn completions(&self, _index: usize, _ctx: &CompletionContext) -> Vec<Candidate> {
        Vec::new()
    }

    /// Problems with a parsed call beyond "an argument didn't match" —
    /// `\repeat volta 0`, a repeat kind that isn't one of the four, a `\volta`
    /// outside any `\repeat`. Called by [`Document::diagnostics`](crate::document::Document::diagnostics)
    /// for every parsed call; the default suits the overwhelming majority of
    /// commands, which have nothing beyond arity to complain about.
    ///
    /// Produces LSP `Diagnostic`s directly rather than going through the note
    /// pass's `Problem` enum. `Problem` is a closed, `Copy` enum of fixed
    /// variants, which suits the note reader's small fixed set of complaints
    /// but not this: command diagnostics are open-ended and command-specific,
    /// and a `SchemeCommand` built at runtime could not add variants to it at
    /// all. `ctx` supplies the span-to-range conversion that keeps this method
    /// from needing a `LineIndex` of its own.
    fn check(
        &self,
        _call: &CommandCall,
        _ctx: &CheckContext,
    ) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        Vec::new()
    }
}

/// One parameter of a command's signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The name from the definition (`weightList`), shown in signature help.
    /// `Cow` so hand-written impls can use literals while parsed ones own
    /// their strings.
    pub name: Cow<'static, str>,
    /// What this parameter looks like in source, and hence how to consume it.
    pub kind: ArgKind,
    /// Whether the parameter may be absent. LilyPond writes these as
    /// `(name default)` pairs and matches them by trying the predicate and
    /// backtracking; we approximate that by trying the shape.
    pub optional: bool,
}

impl Param {
    /// A required parameter — most of them. `name` is a `'static` literal, the
    /// only kind the hand-written signatures ever need.
    const fn required(name: &'static str, kind: ArgKind) -> Self {
        Self {
            name: Cow::Borrowed(name),
            kind,
            optional: false,
        }
    }

    /// An optional parameter — `\relative`'s reference pitch, `\tempo`'s three
    /// pieces, `\lyricsto`'s voice name.
    const fn optional(name: &'static str, kind: ArgKind) -> Self {
        Self {
            name: Cow::Borrowed(name),
            kind,
            optional: true,
        }
    }
}

/// The shape of one argument a command expects, in the order it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgKind {
    /// A bare word naming a variant — the `volta` of `\repeat volta 2`. A
    /// `symbol` in the grammar, meaningful only as this argument (contrast the
    /// `escaped_word` of a command itself).
    BareWord,
    /// A single unsigned integer — the `2` of `\repeat volta 2`.
    Count,
    /// A comma-separated list of unsigned integers — the `2,3` of `\volta 2,3`.
    NumberList,
    /// A time-signature-shaped fraction — the `4/4` of `\time 4/4`. A single
    /// `fraction` token in the grammar (`unsignedInteger '/' unsignedInteger`
    /// lexed as one), not two numbers either side of a `/` punctuation node.
    Fraction,
    /// A music expression: a `{ … }` or `<< … >>` block, or a single braceless
    /// note or chord (`\repeat percent 4 c2`).
    Music,
    /// A note name with octave marks — `\relative c'`, `\fixed c`, the tonic of
    /// `\key c \major`. Resolved through the [`Language`] active at the point
    /// of the call, the same way an ordinary note is resolved.
    Pitch,
    /// An `escaped_word` used as an argument value rather than a command in its
    /// own right — the `\major` of `\key c \major`.
    Word,
    /// A quoted string or a bare symbol standing in for one — `\clef "bass"`,
    /// `\language english`.
    String,
    /// A context-property path: symbols joined by `.` — `\set
    /// Staff.instrumentName`.
    PropertyPath,
    /// A predicate we have no source-shape rule for, named so hover and
    /// signature help can still show it. Consumes exactly one node, which is
    /// right often enough to beat refusing the whole signature.
    Unknown(Cow<'static, str>),
    /// A fixed punctuation token that belongs to the signature itself rather
    /// than to any value the caller supplies — the `=` of `\tempo 4 = 120`,
    /// `\new Staff = "id"` and `\change Staff = "lower"`. Consumes a
    /// `punctuation` node whose text matches `text` exactly, so a bespoke
    /// [`parse_args`](Command::parse_args) can read it through the same
    /// [`ArgReader::take`] every other argument goes through, rather than a
    /// method of its own.
    Literal(&'static str),
    /// A run of parameters that belong together — present as a unit or not at
    /// all — rather than each independently optional: `\new type [= name]`'s
    /// `=` and `name`, `\tempo [text] [duration = value]`'s `duration`, `=`
    /// and `value`. Lets [`default_parse`] itself decide whether to consume
    /// the whole clause, rather than every command whose grammar pairs an
    /// `=` with something needing its own hand-written
    /// [`parse_args`](Command::parse_args).
    ///
    /// Matched by [`consume_group`], prefix-preserving: a required piece that
    /// fails partway through simply stops the group's own walk, keeping
    /// whatever prefix already matched, rather than failing the group outright
    /// — `\tempo 4` (duration read, `=` not yet typed) still matches, as a
    /// one-piece group. Only an entirely-unmatched first piece makes the whole
    /// group absent, which is what lets the *outer* [`Param::optional`] skip
    /// past it cleanly. See [`consume_group`] for why this preserves the
    /// `a_truncated_call_yields_a_prefix_of_the_complete_calls_arguments`
    /// property test's invariant, at the cost of `Arg::Group` sometimes
    /// holding fewer pieces than the `ArgKind::Group` that matched it lists.
    Group(&'static [Param]),
    /// The context type named by `\new`/`\context` — `Staff` in `\new Staff`.
    /// A `symbol`, same shape as [`BareWord`](ArgKind::BareWord), but kept as
    /// its own kind because it names a different namespace (context types,
    /// not commands): go-to-definition and semantic highlighting need to
    /// treat it differently from an ordinary bare word, which
    /// [`Arg::ContextType`] carries the name and span for.
    ContextType,
    /// A context *instance* name — `"vocals"` in `\new Voice = "vocals"`,
    /// `\lyricsto "vocals"` or `\change Staff = "vocals"`. A quoted `string`
    /// or a bare `symbol`, the same two shapes [`String`](ArgKind::String)
    /// accepts, but kept as its own kind for the same reason
    /// [`ContextType`](ArgKind::ContextType) is kept apart from
    /// [`BareWord`](ArgKind::BareWord): it names the context-instance
    /// namespace ([`ContextInstance`](crate::context::ContextInstance)),
    /// which go-to-definition and semantic highlighting need to resolve
    /// differently from an arbitrary string.
    ///
    /// Completion inserts a quoted value (`command_assist::completion_item`
    /// treats this the same as [`String`](ArgKind::String)) even though a
    /// bare symbol parses identically — every real example in LilyPond's own
    /// manual quotes an instance name, and quoting is what tells a reader at
    /// a glance that the word names an instance rather than a context type
    /// (`Staff`, unquoted) or an ordinary bare word.
    ContextName,
}

/// How music inside a command's body is to be read: whether its symbols are
/// events at all, how their octaves are written, and the note names they are
/// spelled in.
///
/// The three travel together because [`Command::music_context`] is the one
/// place any of them can change, and a command that changes one usually
/// leaves the others alone: `\chordmode` re-reads the symbols but keeps the
/// octave entry and the language, `\language` changes the language and reads
/// the music no differently. Every impl builds its answer from the `ambient`
/// context it is handed, so inheriting is the default and replacing is the
/// deliberate act — which is why there is no `Inherit`: a command that
/// establishes nothing returns `ambient` unchanged, as
/// [`Command::music_context`]'s default body does.
///
/// This is exactly the state the note analyser's walk carries, in the same
/// shape, so the two hand it back and forth by field rather than through a
/// conversion. It is not, however, all of one *reach*: an entry mode and a
/// region govern the body and stop at its closing brace, while a language
/// change outlives it, LilyPond's parser switching note names for the rest of
/// the parse. Which half goes where is the analyser's business; see
/// [`note_analyser`](crate::note_analyser)'s `handle_command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicContext {
    /// How the octaves of the symbols here are written.
    pub entry: NoteEntry,
    /// Whether the symbols here are note events, chord-mode entries, or not
    /// events at all. Never [`Region::NoteContext`] in a context handed to a
    /// [`Command`]: see [`Region::in_a_command_body`].
    pub region: Region,
    /// The note names those symbols are spelled in, as `\language` last left it.
    pub language: Language,
}

impl MusicContext {
    /// A context reading `region`'s symbols `entry`-wise in `language`.
    pub fn new(entry: NoteEntry, region: Region, language: Language) -> Self {
        Self {
            entry,
            region,
            language,
        }
    }

    /// This context with a different [`NoteEntry`] — and, with it,
    /// [`Region::NoteMusic`], since saying how octaves are written is only
    /// meaningful where the symbols are notes. What `\relative`, `\fixed` and
    /// `\notemode` return for their bodies.
    #[must_use]
    pub fn with_entry(&self, entry: NoteEntry) -> Self {
        Self {
            entry,
            region: Region::NoteMusic,
            language: self.language.clone(),
        }
    }

    /// This context with a different [`Region`], keeping the octave entry —
    /// what `\chordmode` and the non-note commands (`\lyricmode`, `\header`,
    /// …) return. The entry mode is kept rather than reset because a nested
    /// `{ … }` that returns to note music should still be read the way the
    /// enclosing `\relative` was.
    #[must_use]
    pub fn with_region(&self, region: Region) -> Self {
        Self {
            entry: self.entry,
            region,
            language: self.language.clone(),
        }
    }

    /// This context with a different [`Language`], keeping how the music is
    /// read — what [`language`] returns for a `\language` that named one it
    /// knows.
    #[must_use]
    pub fn with_language(&self, language: Language) -> Self {
        Self {
            entry: self.entry,
            region: self.region,
            language,
        }
    }
}

/// How the octaves of written notes are to be read — LilyPond's "octave
/// entry", extended with nothing: the three modes here are the three it has.
///
/// Carried by [`MusicContext`] and by the analyser's walk alike, which is why
/// [`Fixed`](Self::Fixed) keeps the `i32` the analyser's octave arithmetic
/// works in rather than being narrowed for the trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteEntry {
    /// Octave marks are absolute: `c` is `octave -1`, and each `'`/`,`
    /// adjusts from there.
    Absolute,
    /// `\relative`: octave marks adjust from the previous note, whose octave
    /// is otherwise the nearest to it. Carries the running reference pitch.
    Relative(Pitch),
    /// `\fixed p`: like absolute, but shifted so an unmarked note sits in
    /// `p`'s octave. Carries that octave offset.
    Fixed(i32),
}

/// What the symbols in a stretch of music *mean* — whether they are note
/// events, chord-mode entries, or not events at all.
///
/// Independent of [`NoteEntry`], which says how a note's octave is written:
/// `\chordmode` inside a `\relative` changes what a bare symbol means without
/// disturbing how the octaves of the notes around it are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// Bare symbols and chords here are read as note events; nested bare
    /// blocks stay note-music.
    NoteMusic,
    /// `\chordmode`: bare symbols here are chord-mode entries (a root with a
    /// `:quality`/`/bass`), read for their extent and duration but not their
    /// pitch; nested bare blocks stay chord-music.
    ChordMusic,
    /// `\lyricmode`/`\lyrics`/`\addlyrics`/`\lyricsto`, and a `Lyrics`
    /// context: a word-valued event stream. Every bare symbol and quoted
    /// string here is a syllable — an event in its own right, carrying an
    /// optional duration like any other, but never resolved to a pitch;
    /// nested bare blocks stay lyrics.
    ///
    /// Drums and figures want exactly this treatment with a different
    /// vocabulary, and should become their own regions rather than special
    /// cases: everything downstream reads
    /// [`reads_words`](Self::reads_words), not the variant.
    Lyrics,
    /// Drums, figures, markup, headers — scanned for nested music and
    /// directives, but bare symbols are not events, and nested bare blocks
    /// stay non-note.
    NonNote,
    /// Not itself an event stream, though its nested bare blocks are
    /// note-music. The top level of a file, and nothing else: a command's own
    /// body is always a concrete event stream, so this never reaches a
    /// [`Command`] — see [`in_a_command_body`](Self::in_a_command_body).
    NoteContext,
}

impl Region {
    /// The region a nested bare block (one with no governing command)
    /// inherits.
    pub fn nested_block(self) -> Region {
        match self {
            Region::NonNote => Region::NonNote,
            Region::ChordMusic => Region::ChordMusic,
            Region::Lyrics => Region::Lyrics,
            Region::NoteMusic | Region::NoteContext => Region::NoteMusic,
        }
    }

    /// Whether a bare `symbol` or quoted `string` here is an event in its own
    /// right, with a word where a note has a pitch: a lyric syllable now, a
    /// drum name or a figure once those grow their own regions.
    ///
    /// The analyser and everything reading its events branch on this rather
    /// than on [`Lyrics`](Self::Lyrics) itself, so adding the next word-valued
    /// mode is one variant and one arm here, not a search for every place
    /// lyrics were named.
    pub fn reads_words(self) -> bool {
        matches!(self, Region::Lyrics)
    }

    /// This region as a command's body sees it. The one and only difference
    /// is [`NoteContext`](Self::NoteContext): a command's music argument is
    /// an event stream even when the command was written at the top level,
    /// where nothing else is.
    pub fn in_a_command_body(self) -> Region {
        match self {
            Region::NoteContext => Region::NoteMusic,
            concrete => concrete,
        }
    }
}

/// A cursor over the sibling nodes following a command keyword, shared by
/// [`default_parse`] and by hand-written overrides so both consume arguments
/// the same way.
pub struct ArgReader<'a> {
    children: &'a [Node<'a>],
    src: &'a str,
    /// The note-name language active where this call was found, needed to
    /// resolve an [`ArgKind::Pitch`] the same way an ordinary note is resolved.
    language: &'a Language,
    /// The index of the first unconsumed sibling.
    next: usize,
}

impl<'a> ArgReader<'a> {
    fn new(children: &'a [Node<'a>], start: usize, src: &'a str, language: &'a Language) -> Self {
        Self {
            children,
            src,
            language,
            next: start,
        }
    }

    /// Consumes one argument of `kind`, or returns `None` **without
    /// advancing**. The non-consuming failure is what lets an optional
    /// parameter be retried against the next parameter; overrides must
    /// preserve it.
    pub fn take(&mut self, kind: &ArgKind) -> Option<Arg> {
        let (arg, next) = consume_arg(kind, self.children, self.next, self.src, self.language)?;
        self.next = next;
        Some(arg)
    }

    /// The next node without consuming it, for overrides that need to look
    /// before they leap.
    pub fn peek(&self) -> Option<Node<'a>> {
        self.children.get(self.next).copied()
    }

    /// The source text of the next unconsumed node, for overrides that need
    /// to recognise a fixed keyword before deciding how to consume it —
    /// [`new_context`](super::new_context) checking whether an optional
    /// `\with { … }` block follows a context type, the one case so far that
    /// needs to look at more than a node's kind.
    pub fn peek_text(&self) -> Option<&'a str> {
        self.peek()
            .map(|node| &self.src[node.start_byte()..node.end_byte()])
    }

    /// Consumes the next node whatever its kind, returning its span, or
    /// `None` at the end of the stream. For a bespoke
    /// [`parse_args`](Command::parse_args) that needs to step over a node
    /// without asking [`ArgKind`]'s shape rules to make sense of it —
    /// [`new_context`](super::new_context) uses this for the `\with`
    /// keyword and its block, which must be consumed but must *not* become
    /// an [`Arg::Music`] (the note analyser would then misread the `\with`
    /// block's property settings as notes) or an [`Arg::Unknown`] read
    /// through the ordinary [`ArgKind::Unknown`] path (which declines
    /// music-shaped nodes on purpose — see the comment in
    /// [`consume_arg`]).
    pub fn skip_one(&mut self) -> Option<Span> {
        let node = *self.children.get(self.next)?;
        self.next += 1;
        Some(node_span(node))
    }

    /// The index of the first unconsumed sibling — what [`parse`] returns to
    /// its caller.
    pub fn position(&self) -> usize {
        self.next
    }
}

/// Walks `signature`, trying each [`Param`]'s [`ArgKind`] against `args` in
/// order. A required parameter that fails to match stops consumption, so a
/// half-typed `\repeat volta` yields the arguments seen so far rather than
/// nothing; an optional parameter that fails is skipped (its failure doesn't
/// advance `args`) and the next parameter is tried against the same node.
pub fn default_parse(signature: &[Param], args: &mut ArgReader) -> Vec<Arg> {
    let mut out = Vec::new();
    for param in signature {
        match args.take(&param.kind) {
            Some(arg) => out.push(arg),
            None if param.optional => continue,
            None => break,
        }
    }
    out
}

/// Renders a command's signature as `\name param [optional]`, the label shared
/// by signature help and the default [`synopsis`](Command::synopsis) — an
/// optional [`Param`] shown in brackets, as LilyPond's own manual does.
pub fn signature_label(name: &str, params: &[Param]) -> String {
    let mut label = format!("\\{name}");
    for param in params {
        label.push(' ');
        if param.optional {
            label.push('[');
            label.push_str(&param.name);
            label.push(']');
        } else {
            label.push_str(&param.name);
        }
    }
    label
}

/// `code`, fenced for Markdown, for a [`synopsis`](Command::synopsis) to
/// return.
pub fn code_block(code: &str) -> String {
    format!("```lilypond\n{code}\n```")
}

/// Every place `command`'s file binds its name, in source order.
///
/// Walks the [`redefines`](Command::redefines) chain, so a file that assigns
/// `foo` twice yields both spans from the single layer entry `\foo` resolves
/// to. Empty for a command with no definition to point at — every builtin, and
/// (later) whatever the install layer can't locate.
pub fn definition_spans(command: &dyn Command) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut current = Some(command);
    while let Some(command) = current {
        spans.extend(command.definition());
        current = command.redefines().map(Arc::as_ref);
    }
    // The chain runs latest-first; every caller wants source order.
    spans.reverse();
    spans
}

/// Hover text for a command.
pub struct Documentation {
    /// Markdown, ready for an LSP `MarkupContent`.
    pub markdown: String,
}

/// A value worth completing at one parameter position — one of a command's
/// closed set of accepted words, such as `\repeat`'s four kinds or `\key`'s
/// mode names.
///
/// Kept as a plain label/documentation pair rather than an LSP
/// `CompletionItem`: whether the value needs a leading backslash on insertion
/// (an [`ArgKind::Word`] does, a [`ArgKind::BareWord`] doesn't), or a pair of
/// quotes (an [`ArgKind::String`]), depends on the parameter it fills, not on
/// the candidate itself, so that decision belongs to the completion feature
/// that renders these, not to `Command` impls.
///
/// [`Cow`] because nearly every candidate is written out in a `static` table
/// here, while a few — the install's number, offered by [`version`] — are
/// known only once the workspace has loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The word as written in source, without a leading backslash even where
    /// the parameter's [`ArgKind`] is [`Word`](ArgKind::Word), and without
    /// quotes where it is [`String`](ArgKind::String).
    pub label: Cow<'static, str>,
    /// A short, one-line description shown alongside the label.
    pub documentation: Cow<'static, str>,
}

impl Candidate {
    /// A candidate spelled out in a table here, both halves `'static`.
    pub const fn new(label: &'static str, documentation: &'static str) -> Self {
        Self {
            label: Cow::Borrowed(label),
            documentation: Cow::Borrowed(documentation),
        }
    }
}

/// Whether `name` is one of LilyPond's `Internal*` context types —
/// `InternalGregorianStaff` since 2.24, joined by `InternalMensuralStaff` in
/// 2.26 (checked against real installs of both: `grep '\\name Internal'
/// engraver-init.ly`). Each exists only as an intermediate base the real
/// Gregorian/Mensural notation context types inherit from — `VaticanaStaff`,
/// `MensuralStaff` and their kin `\InternalGregorianStaff`/
/// `\InternalMensuralStaff` themselves rather than being written by a score —
/// so no real `\new`/`\context` ever names one directly. Every match so far
/// is `Internal`-prefixed, so a plain prefix test stands in for a
/// hand-maintained name list a future LilyPond release could silently add
/// to.
///
/// Used only to decide what completion *offers* — see
/// [`context_type_candidates`]. [`Scope::get_context_type`] and
/// [`Scope::is_known`] still resolve these names, so a user's own context
/// type that inherits from one (as `VaticanaStaff` does) is never flagged as
/// referencing something undefined.
fn is_internal_context_type(name: &str) -> bool {
    name.starts_with("Internal")
}

/// Every context type `scope` can see, worth offering after `\new`/`\context`
/// — [`Scope::visible_context_types`], filtered to leave out
/// [`is_internal_context_type`] names, and rendered as a [`Candidate`]:
/// the type's own name, documented with its `\description` where it has one
/// (already converted from Texinfo to Markdown by
/// [`context::read`](crate::context::read)) or nothing where it doesn't — a
/// user's own `\context { \name MyStaff }` need carry no `\description` to be
/// offered.
///
/// Shared by [`new_context`] and [`change`], the two commands whose first
/// parameter names a context type.
fn context_type_candidates(scope: &Scope) -> Vec<Candidate> {
    scope
        .visible_context_types()
        .into_iter()
        .filter(|(name, _)| !is_internal_context_type(name))
        .map(|(name, known)| Candidate {
            label: Cow::Owned(name.to_string()),
            documentation: known
                .value
                .description
                .clone()
                .map(Cow::Owned)
                .unwrap_or(Cow::Borrowed("")),
        })
        .collect()
}

/// Every context instance name `scope` can see, worth offering at `\change`,
/// `\lyricsto` and `\new`/`\context`'s `= "name"` position —
/// [`Scope::visible_context_instances`], rendered as a [`Candidate`]: the
/// instance's own name, documented with the context type it was created as
/// where [`ContextInstance::type_name`](crate::context::ContextInstance::type_name)
/// recorded one (`None` for a half-typed `\new = "vocals"` with no type
/// written yet).
///
/// Shared by [`new_context`], [`change`] and [`lyricsto`], the three commands
/// with a parameter in this namespace.
fn context_instance_candidates(scope: &Scope) -> Vec<Candidate> {
    scope
        .visible_context_instances()
        .into_iter()
        .map(|(name, known)| Candidate {
            label: Cow::Owned(name.to_string()),
            documentation: known
                .value
                .type_name
                .clone()
                .map(Cow::Owned)
                .unwrap_or(Cow::Borrowed("")),
        })
        .collect()
}

/// What a [`Command`] may consult when asked for completions: the knowledge
/// that belongs to the workspace rather than to the command.
///
/// Handed down from [`DocumentGraph`](crate::document_graph::DocumentGraph)
/// rather than read here, because the hand-written commands are built once, by
/// a `LazyLock` that runs long before any client says which LilyPond it means.
pub struct CompletionContext<'a> {
    /// The version of the installation the workspace was loaded from
    /// (`2.24.3`), or `None` when the client named none — see
    /// [`install::version`](crate::install::version).
    pub lilypond_version: Option<&'a str>,
    /// The document's own [`Scope`] — what [`new_context`] and [`change`]
    /// read [`visible_context_types`](Scope::visible_context_types) and
    /// [`visible_context_instances`](Scope::visible_context_instances) from
    /// to compute their candidates, and what
    /// [`command_assist`](crate::command_assist)'s text-level fallback
    /// (for a `\new`/`\context` with nothing typed after it yet — see the
    /// module doc there) looks a bare keyword up in. The caller must pass
    /// the *same* document's scope this context is being built for: nothing
    /// here checks that they match.
    pub scope: &'a Scope,
}

/// What a command may consult while checking a call, and the means to report
/// what it finds.
///
/// Built once per [`Document::diagnostics`](crate::document::Document::diagnostics)
/// call and handed to every call's [`Command::check`] in turn, so `lines` (for
/// converting a [`Span`] to the LSP [`Range`](tower_lsp::lsp_types::Range) a
/// [`Diagnostic`] needs) and `calls` (the whole document's calls, for
/// [`enclosing`](Self::enclosing)) are computed once rather than per call.
pub struct CheckContext<'a> {
    lines: &'a LineIndex,
    calls: &'a Commands,
}

impl<'a> CheckContext<'a> {
    pub(crate) fn new(lines: &'a LineIndex, calls: &'a Commands) -> Self {
        Self { lines, calls }
    }

    /// The innermost call whose body contains `span` — what `\volta` asks to
    /// discover it has no `\repeat`. "Innermost" is the covering call with the
    /// latest keyword, matching [`Commands::call_site_at`]'s notion of nesting.
    pub fn enclosing(&self, span: Span) -> Option<&CommandCall> {
        self.calls
            .iter()
            .filter(|call| {
                call.body()
                    .is_some_and(|body| body.start <= span.start && span.end <= body.end)
            })
            .max_by_key(|call| call.keyword.start)
    }

    /// Builds a [`Diagnostic`] at `span`, converting it to a range and filling
    /// in `source: "ly-lsp"` so every impl reports consistently with the rest
    /// of the server's diagnostics. Prefer these over constructing a
    /// `Diagnostic` literal.
    pub fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        self.diagnostic(span, DiagnosticSeverity::ERROR, message)
    }

    pub fn warning(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        self.diagnostic(span, DiagnosticSeverity::WARNING, message)
    }

    fn diagnostic(
        &self,
        span: Span,
        severity: DiagnosticSeverity,
        message: impl Into<String>,
    ) -> Diagnostic {
        Diagnostic {
            range: self.lines.range_of(span),
            severity: Some(severity),
            source: Some("ly-lsp".to_string()),
            message: message.into(),
            ..Diagnostic::default()
        }
    }
}

/// A parsed command invocation: the command word and the arguments it consumed,
/// each with the source extent it covered.
#[derive(Clone)]
pub struct CommandCall {
    /// The command name, without the leading backslash (`repeat`).
    pub name: String,
    /// The `\command` keyword token.
    pub keyword: Span,
    /// The whole call: the keyword through the last argument actually consumed.
    pub span: Span,
    /// The arguments parsed, in order. Shorter than the signature when the source
    /// is mid-edit and an argument is still missing.
    pub args: Vec<Arg>,
    /// The [`Command`] impl that `name` resolved to during parsing — the same
    /// whichever layer of the [`Scope`](crate::vocabulary::Scope) resolved
    /// the name — see [`Scope::get`](crate::vocabulary::Scope::get). Carried
    /// here so a caller that already has a `CommandCall` never needs a second
    /// name lookup to ask it anything.
    pub cmd: Arc<dyn Command>,
    /// Where the layer that resolved the name says its knowledge came from —
    /// see [`Layer::origin`](crate::vocabulary::Layer::origin). Copied out of
    /// the layer rather than holding the layer itself, because this is all a
    /// call ever wants of it.
    pub origin: Arc<str>,
}

impl std::fmt::Debug for CommandCall {
    /// Hand-written because [`cmd`](Self::cmd) is a `dyn Command`, which
    /// doesn't implement `Debug`; every other field prints as `#[derive]`
    /// would, and `cmd` prints as its name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandCall")
            .field("name", &self.name)
            .field("keyword", &self.keyword)
            .field("span", &self.span)
            .field("args", &self.args)
            .field("cmd", &self.cmd.name())
            .finish()
    }
}

impl CommandCall {
    /// The header span: the keyword and every non-[`Music`](Arg::Music) argument
    /// before the body — the `\repeat volta 2` a cursor sits in to invoke an
    /// action, excluding the `{ … }` body.
    pub fn header(&self) -> Span {
        let end = self
            .args
            .iter()
            .take_while(|arg| !matches!(arg, Arg::Music { .. }))
            .map(Arg::span)
            .last()
            .map_or(self.keyword.end, |span| span.end);
        Span::new(self.keyword.start, end)
    }

    /// The span of this call's first [`Music`](Arg::Music) argument, its body.
    pub fn body(&self) -> Option<Span> {
        self.args.iter().find_map(|arg| match arg {
            Arg::Music { span } => Some(*span),
            _ => None,
        })
    }
}

/// One parsed argument, tagged by its [`ArgKind`] and carrying the source extent
/// it covered together with its decoded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    BareWord {
        span: Span,
        text: String,
    },
    Count {
        span: Span,
        value: u32,
    },
    NumberList {
        span: Span,
        values: Vec<u32>,
    },
    /// An [`ArgKind::Fraction`] argument: the value isn't needed anywhere yet
    /// (see [`Arg::Unknown`]/[`Arg::Literal`]), so only the span is kept.
    Fraction {
        span: Span,
    },
    Music {
        span: Span,
    },
    Pitch {
        span: Span,
        pitch: Pitch,
    },
    Word {
        span: Span,
        text: String,
    },
    String {
        span: Span,
        text: String,
    },
    PropertyPath {
        span: Span,
        path: Vec<String>,
    },
    /// An [`ArgKind::Unknown`] argument: one node, consumed but not interpreted.
    Unknown {
        span: Span,
    },
    /// An [`ArgKind::Literal`] argument: a fixed punctuation token, carrying
    /// nothing beyond its span since its text is already known from the
    /// [`ArgKind`] that matched it.
    Literal {
        span: Span,
    },
    /// An [`ArgKind::Group`] argument: the pieces actually matched, in order.
    /// Prefix-preserving (see [`consume_group`]): may hold fewer pieces than
    /// the [`ArgKind::Group`] that matched it lists — `\tempo 4` with no `=`
    /// yet typed is one [`Arg::Count`] on its own, not the full three-piece
    /// clause — but never zero, since an empty match is `None` from
    /// [`consume_group`] rather than an empty `Group`.
    Group {
        span: Span,
        args: Vec<Arg>,
    },
    /// An [`ArgKind::ContextType`] argument: the context type name a
    /// `\new`/`\context` names, e.g. `Staff`.
    ContextType {
        span: Span,
        name: String,
    },
    /// An [`ArgKind::ContextName`] argument: the context instance name a
    /// `\new`/`\context` names, or that `\lyricsto`/`\change` refers to, e.g.
    /// `vocals`.
    ContextName {
        span: Span,
        name: String,
    },
}

impl Arg {
    pub fn span(&self) -> Span {
        match self {
            Arg::BareWord { span, .. }
            | Arg::Count { span, .. }
            | Arg::NumberList { span, .. }
            | Arg::Fraction { span }
            | Arg::Music { span }
            | Arg::Pitch { span, .. }
            | Arg::Word { span, .. }
            | Arg::String { span, .. }
            | Arg::PropertyPath { span, .. }
            | Arg::Unknown { span }
            | Arg::Literal { span }
            | Arg::Group { span, .. }
            | Arg::ContextType { span, .. }
            | Arg::ContextName { span, .. } => *span,
        }
    }
}

/// `args` and, one level deep, whatever an [`Arg::Group`] among them holds —
/// the `= "name"` clause `\new`/`\context`/`\change` parse as a single group
/// rather than flat entries. Yields the group itself too, harmlessly, since
/// no caller is looking for that variant. [`ArgKind::Group`] only ever nests
/// one level, so this doesn't need to recurse further.
///
/// Shared by [`semantic_tokens`](crate::semantic_tokens), which tags the
/// arguments it finds, and by [`Document`](crate::document::Document)'s
/// context-reference lookups, which collect them — both want every argument a
/// call actually holds, and neither cares about the nesting.
pub fn flat_args(args: &[Arg]) -> impl Iterator<Item = &Arg> {
    args.iter().flat_map(|arg| {
        let nested: &[Arg] = match arg {
            Arg::Group { args, .. } => args,
            _ => &[],
        };
        std::iter::once(arg).chain(nested.iter())
    })
}

/// Parses the command at `children[start]` against the [`Command`] impl
/// `scope` resolves it to, if it resolves to one, consuming as many of its
/// arguments as are present. `language` is the note-name language active at
/// this point in the source, needed to resolve a [`Pitch`](ArgKind::Pitch)
/// argument the same way an ordinary note is resolved.
///
/// `children[start]` is usually a bare `escaped_word` sibling (`\repeat`,
/// `\clef`, …), but `\new`/`\context` is different: the grammar folds the
/// keyword *and* its context type into one `named_context` node (`\new
/// Staff`, or `\new Staff = "upper"`), while the music argument — and an
/// optional `\with { … }` block — stay ordinary siblings *after* it, not
/// children of it. So a `named_context` is unwrapped here: its own children
/// (the type, and the optional `= "name"`) are read first, then the reader
/// carries on into the siblings following `children[start]`, as if the two
/// runs were one flat stream. [`new_context::NewContextCommand`] then sees
/// exactly the shape [`default_parse`] expects everywhere else.
///
/// Returns the structured call and the index of the first node after the
/// arguments consumed — into the *original* `children`, regardless of which
/// shape was matched, so the caller (`note_analyser`'s walk) never needs to
/// know the difference. `None` when the word names no command `scope` has a
/// signature for, leaving the caller to handle it as it did before (in
/// `note_analyser`, that means the following block is read as an ordinary bare
/// block).
pub fn parse(
    children: &[Node],
    start: usize,
    src: &str,
    language: &Language,
    scope: &Scope,
) -> Option<(CommandCall, usize)> {
    let node = *children.get(start)?;
    match node.kind() {
        "escaped_word" => parse_call(node, children, start + 1, src, language, scope, |pos| pos),
        "named_context" => {
            let mut cursor = node.walk();
            let inner: Vec<Node> = node.children(&mut cursor).collect();
            let keyword_node = *inner.first()?;
            let inner_rest = &inner[1..];
            let inner_len = inner_rest.len();
            let flattened: Vec<Node> = inner_rest
                .iter()
                .copied()
                .chain(children[start + 1..].iter().copied())
                .collect();
            // A position inside the flattened stream at or before `inner_len`
            // never advanced past the `named_context` node's own children, so
            // — from the outer walk's point of view, which sees the whole
            // `named_context` as a single node at `start` — the next node to
            // read is always `start + 1`, however much or little of the type
            // and its `= "name"` was actually consumed. Only a position past
            // `inner_len` has stepped into the real siblings (the `\with`
            // block, the music), which map back by subtracting the same
            // offset that was added to reach them.
            parse_call(
                keyword_node,
                &flattened,
                0,
                src,
                language,
                scope,
                move |pos| {
                    if pos <= inner_len {
                        start + 1
                    } else {
                        start + 1 + (pos - inner_len)
                    }
                },
            )
        }
        _ => None,
    }
}

/// The shared second half of [`parse`]: resolves `keyword_node` in `scope`,
/// builds an [`ArgReader`] over `children` starting at `start`, and parses
/// its arguments. `map_next` translates the reader's final position — an
/// index into `children` — back into an index into whatever `children` the
/// caller actually owns, which for the plain `escaped_word` case is the
/// identity function and for `named_context` is the arithmetic [`parse`]
/// documents above.
fn parse_call(
    keyword_node: Node,
    children: &[Node],
    start: usize,
    src: &str,
    language: &Language,
    scope: &Scope,
    map_next: impl Fn(usize) -> usize,
) -> Option<(CommandCall, usize)> {
    if keyword_node.kind() != "escaped_word" {
        return None;
    }
    let name = src[keyword_node.start_byte()..keyword_node.end_byte()].strip_prefix('\\')?;
    let known = scope.get(name)?;
    let (cmd, origin) = (Arc::clone(known.value), Arc::clone(known.layer.origin()));

    let keyword = node_span(keyword_node);
    let mut reader = ArgReader::new(children, start, src, language);
    let args = cmd.parse_args(&mut reader);
    let end = args.last().map_or(keyword.end, |arg| arg.span().end);
    Some((
        CommandCall {
            name: name.to_string(),
            keyword,
            span: Span::new(keyword.start, end),
            args,
            cmd,
            origin,
        },
        map_next(reader.position()),
    ))
}

/// Consumes one argument of the given `kind` starting at `children[i]`,
/// returning it and the next index, or `None` if the expected node isn't
/// there. `None` never advances `i` — the caller sees the same `i` it passed
/// in.
fn consume_arg(
    kind: &ArgKind,
    children: &[Node],
    i: usize,
    src: &str,
    language: &Language,
) -> Option<(Arg, usize)> {
    let node = *children.get(i)?;
    match kind {
        ArgKind::BareWord if node.kind() == "symbol" => {
            let span = node_span(node);
            let text = src[span.start..span.end].to_string();
            Some((Arg::BareWord { span, text }, i + 1))
        }
        ArgKind::ContextType if node.kind() == "symbol" => {
            let span = node_span(node);
            let name = src[span.start..span.end].to_string();
            Some((Arg::ContextType { span, name }, i + 1))
        }
        ArgKind::Count if node.kind() == "unsigned_integer" => {
            let span = node_span(node);
            let value = src[span.start..span.end].parse().unwrap_or(0);
            Some((Arg::Count { span, value }, i + 1))
        }
        ArgKind::NumberList => consume_number_list(children, i, src),
        ArgKind::Fraction if node.kind() == "fraction" => Some((
            Arg::Fraction {
                span: node_span(node),
            },
            i + 1,
        )),
        ArgKind::Music if is_block(node.kind()) => Some((
            Arg::Music {
                span: node_span(node),
            },
            i + 1,
        )),
        ArgKind::Music => consume_single_music(children, i),
        ArgKind::Pitch => consume_pitch(children, i, src, language),
        ArgKind::Word if node.kind() == "escaped_word" => {
            let span = node_span(node);
            let text = src[span.start..span.end]
                .strip_prefix('\\')
                .unwrap_or(&src[span.start..span.end])
                .to_string();
            Some((Arg::Word { span, text }, i + 1))
        }
        ArgKind::String => consume_string(children, i, src),
        ArgKind::Literal(text) if is_punct(node, src, text) => Some((
            Arg::Literal {
                span: node_span(node),
            },
            i + 1,
        )),
        ArgKind::ContextName => consume_context_name(children, i, src),
        ArgKind::PropertyPath => consume_property_path(children, i, src),
        ArgKind::Group(sub_params) => consume_group(sub_params, children, i, src, language),
        // `Unknown` must never claim a node `ArgKind::Music` or
        // `ArgKind::Fraction` would also claim. It has no shape check of its
        // own — that's the whole point of it — so an *optional* `Unknown`
        // parameter would otherwise consume whatever sits next
        // unconditionally, including the real argument that follows when the
        // optional one was simply omitted. `\tuplet 3/2 { c d e }` is the
        // case that matters most for music: the (optional, unmapped)
        // tuplet-span predicate sits directly before the required music, and
        // every real score omits the span. Declining here is what makes that
        // `Unknown` parameter fail to match instead of swallowing the block
        // whole, so `default_parse` skips it (it's optional) and tries the
        // block against `music` instead, where it belongs. `\time`'s own
        // beat-structure predicate — unmapped as of LilyPond 2.26, having
        // been renamed from `number-list?` — is the equivalent case for
        // `Fraction`: without this, it would swallow the `4/4` meant for the
        // required `fraction` parameter right after it. See
        // `looks_like_a_typed_shape`.
        ArgKind::Unknown(_) if !looks_like_a_typed_shape(node.kind()) => Some((
            Arg::Unknown {
                span: node_span(node),
            },
            i + 1,
        )),
        _ => None,
    }
}

/// Consumes an [`ArgKind::Group`]: a run of `sub_params`, walked the same way
/// [`default_parse`] walks a whole signature — a required piece that fails
/// stops the walk rather than failing the group outright — but starting from
/// `children[start]` instead of wherever an [`ArgReader`] happens to be, and
/// returning what it matched as one [`Arg::Group`] instead of extending a
/// caller's `Vec` directly.
///
/// Prefix-preserving: `\tempo 4` (duration read, no `=` yet) still returns
/// `Some` — a group of one, `[duration]` — because `sub_params`' first piece
/// (`duration`) matched; only when *that* first piece fails to match at all
/// does the whole group fail, returning `None` for the [`Param::optional`]
/// that wraps it to skip past cleanly. Without this, a required piece
/// failing partway through would have to erase pieces already matched to
/// report "the group didn't happen", which would make a half-typed `\tempo
/// 4` report no arguments at all — a regression from today's hand-written
/// equivalent, and a break of the `a_truncated_call_yields_a_prefix_of_the_
/// complete_calls_arguments` property test's invariant that a truncated call
/// yields a prefix of the complete one's arguments.
fn consume_group(
    sub_params: &[Param],
    children: &[Node],
    start: usize,
    src: &str,
    language: &Language,
) -> Option<(Arg, usize)> {
    let mut args = Vec::new();
    let mut i = start;
    for param in sub_params {
        match consume_arg(&param.kind, children, i, src, language) {
            Some((arg, next)) => {
                i = next;
                args.push(arg);
            }
            None if param.optional => continue,
            None => break,
        }
    }
    let first = args.first()?;
    let end = args.last().map_or(first.span().end, |arg| arg.span().end);
    let span = Span::new(first.span().start, end);
    Some((Arg::Group { span, args }, i))
}

/// Whether `kind` is a node some more specific [`ArgKind`] would itself
/// consume — [`Music`](ArgKind::Music)'s `{ … }`/`<< … >>` block or braceless
/// note/chord, or [`Fraction`](ArgKind::Fraction)'s `fraction` token.
/// [`ArgKind::Unknown`] must decline these — see the comment where it's
/// matched in [`consume_arg`].
fn looks_like_a_typed_shape(kind: &str) -> bool {
    is_block(kind) || kind == "symbol" || kind == "chord" || kind == "fraction"
}

/// Consumes a braceless music argument — a single note or chord written without
/// surrounding braces, as in `\repeat percent 4 c2`. The grammar leaves a note's
/// tokens as a flat run of byte-adjacent siblings (`c`, `2`, `.`, `->`, …) with
/// the next event whitespace-separated, so we take the leading `symbol`/`chord`
/// and every sibling butting directly against it. `None` if the next node is
/// neither a note symbol nor a chord (a block is handled before this).
fn consume_single_music(children: &[Node], start: usize) -> Option<(Arg, usize)> {
    let first = *children.get(start)?;
    if first.kind() != "symbol" && first.kind() != "chord" {
        return None;
    }
    let mut end = first.end_byte();
    let mut i = start + 1;
    while let Some(node) = children.get(i).filter(|n| n.start_byte() == end) {
        end = node.end_byte();
        i += 1;
    }
    Some((
        Arg::Music {
            span: Span::new(first.start_byte(), end),
        },
        i,
    ))
}

/// Consumes a comma-separated run of unsigned integers (`2,3`) starting at
/// `children[start]`, or `None` if the first node isn't an integer.
fn consume_number_list(children: &[Node], start: usize, src: &str) -> Option<(Arg, usize)> {
    let first = *children.get(start)?;
    if first.kind() != "unsigned_integer" {
        return None;
    }
    let mut values = vec![number(first, src)];
    let mut end = first.end_byte();
    let mut i = start + 1;
    // Each further `, n` extends the list; a trailing comma with no integer is
    // left for the caller.
    while children.get(i).is_some_and(|n| is_comma(*n, src))
        && let Some(n) = children
            .get(i + 1)
            .filter(|n| n.kind() == "unsigned_integer")
    {
        values.push(number(*n, src));
        end = n.end_byte();
        i += 2;
    }
    Some((
        Arg::NumberList {
            span: Span::new(first.start_byte(), end),
            values,
        },
        i,
    ))
}

/// Consumes a pitch argument — a note name with octave marks, resolved through
/// `language` the same way an ordinary note is (`\relative c'`, `\fixed c`, the
/// tonic of `\key c \major`). `None` if the node isn't a `symbol`, or isn't a
/// note name `language` knows.
fn consume_pitch(
    children: &[Node],
    start: usize,
    src: &str,
    language: &Language,
) -> Option<(Arg, usize)> {
    let node = *children.get(start)?;
    if node.kind() != "symbol" {
        return None;
    }
    let text = &src[node.start_byte()..node.end_byte()];
    let (note_name, alteration) = language.note(text)?;
    let mut i = start + 1;
    let (marks, check) = parse_octave_marks(children, &mut i, src);
    let octave = check.unwrap_or(marks - 1);
    let end = children.get(i - 1).map_or(node.end_byte(), Node::end_byte);
    Some((
        Arg::Pitch {
            span: Span::new(node.start_byte(), end),
            pitch: Pitch {
                note_name,
                octave,
                alteration,
            },
        },
        i,
    ))
}

/// Consumes octave marks (`'`/`,`), accidental reminders (`!`/`?`) and an
/// optional octave check (`='`/`=,`) starting at `children[*i]` — the same
/// grammar `note_analyser::Analyser::parse_octave` reads for a note in music,
/// but here for a command's own pitch argument, which needs no [`Language`]:
/// marks carry no note name of their own. Returns the net octave shift, and
/// the checked octave (LilyPond's internal value, `c` = -1) if a check was
/// present.
fn parse_octave_marks(children: &[Node], i: &mut usize, src: &str) -> (i32, Option<i32>) {
    let mut marks = 0;
    while let Some(node) = children.get(*i) {
        if is_punct(*node, src, "'") {
            marks += 1;
        } else if is_punct(*node, src, ",") {
            marks -= 1;
        } else if is_punct(*node, src, "!") || is_punct(*node, src, "?") {
            // Accidental reminders carry no octave information; consumed so
            // they don't stop the run.
        } else {
            break;
        }
        *i += 1;
    }

    let mut check = None;
    if children.get(*i).is_some_and(|n| is_punct(*n, src, "=")) {
        *i += 1;
        let mut checked = 0;
        while let Some(node) = children.get(*i) {
            if is_punct(*node, src, "'") {
                checked += 1;
            } else if is_punct(*node, src, ",") {
                checked -= 1;
            } else {
                break;
            }
            *i += 1;
        }
        check = Some(checked - 1);
    }
    (marks, check)
}

/// Consumes a quoted string or a bare symbol standing in for one (`\clef
/// "bass"`, `\language english`). `None` if the node is neither.
fn consume_string(children: &[Node], start: usize, src: &str) -> Option<(Arg, usize)> {
    let node = *children.get(start)?;
    match node.kind() {
        "string" => {
            let span = node_span(node);
            let text = string_fragment(node, src).unwrap_or_default().to_string();
            Some((Arg::String { span, text }, start + 1))
        }
        "symbol" => {
            let span = node_span(node);
            let text = src[span.start..span.end].to_string();
            Some((Arg::String { span, text }, start + 1))
        }
        _ => None,
    }
}

/// Consumes a context instance name: a quoted string or a bare symbol
/// standing in for one (`\new Voice = "vocals"`, `\new Voice = vocals`,
/// `\lyricsto vocals`), exactly the two shapes [`consume_string`] reads for
/// [`ArgKind::String`] — but producing [`Arg::ContextName`], since the value
/// names the context-instance namespace rather than an arbitrary string.
/// `None` if the node is neither.
fn consume_context_name(children: &[Node], start: usize, src: &str) -> Option<(Arg, usize)> {
    let node = *children.get(start)?;
    match node.kind() {
        "string" => {
            let span = node_span(node);
            let name = string_fragment(node, src).unwrap_or_default().to_string();
            Some((Arg::ContextName { span, name }, start + 1))
        }
        "symbol" => {
            let span = node_span(node);
            let name = src[span.start..span.end].to_string();
            Some((Arg::ContextName { span, name }, start + 1))
        }
        _ => None,
    }
}

/// The text inside a `string` node's quotes, if it has a `string_fragment`.
fn string_fragment<'a>(string_node: Node, src: &'a str) -> Option<&'a str> {
    let mut cursor = string_node.walk();
    string_node
        .named_children(&mut cursor)
        .find(|n| n.kind() == "string_fragment")
        .map(|n| &src[n.start_byte()..n.end_byte()])
}

/// Consumes a context-property path (`Staff.instrumentName`, or a single
/// `instrumentName`). The grammar's shape here differs by which command reads
/// it: `\unset Staff.instrumentName` (no `= value` to follow) leaves a bare
/// `symbol` or `property_expression` (its own `symbol "." symbol` node)
/// directly among the siblings, but `\set Staff.instrumentName = "x"` wraps
/// the same shape in an `assignment_lhs`, since as far as the grammar's
/// concerned `\set` is spelling out an assignment. Both are handled here so
/// one `ArgKind` covers both commands. `None` if `children[start]` is none of
/// these three.
fn consume_property_path(children: &[Node], start: usize, src: &str) -> Option<(Arg, usize)> {
    let node = *children.get(start)?;
    let target = if node.kind() == "assignment_lhs" {
        let mut cursor = node.walk();
        node.children(&mut cursor).next()?
    } else {
        node
    };
    let path: Vec<String> = match target.kind() {
        "symbol" => vec![src[target.start_byte()..target.end_byte()].to_string()],
        "property_expression" => {
            let mut cursor = target.walk();
            target
                .children(&mut cursor)
                .filter(|n| n.kind() == "symbol")
                .map(|n| src[n.start_byte()..n.end_byte()].to_string())
                .collect()
        }
        _ => return None,
    };
    Some((
        Arg::PropertyPath {
            span: node_span(node),
            path,
        },
        start + 1,
    ))
}

fn number(node: Node, src: &str) -> u32 {
    src[node.start_byte()..node.end_byte()].parse().unwrap_or(0)
}

fn node_span(node: Node) -> Span {
    Span::new(node.start_byte(), node.end_byte())
}

/// Whether a node kind is a music block: a `{ … }` expression or a `<< … >>`
/// parallel-music block. The two ways music groups in the grammar.
pub(crate) fn is_block(kind: &str) -> bool {
    kind == "expression_block" || kind == "parallel_music"
}

fn is_comma(node: Node, src: &str) -> bool {
    is_punct(node, src, ",")
}

/// True if `node` is a `punctuation` node whose text is exactly `text`.
fn is_punct(node: Node, src: &str, text: &str) -> bool {
    node.kind() == "punctuation" && &src[node.start_byte()..node.end_byte()] == text
}

static MUSIC_ONLY_PARAMS: &[Param] = &[Param::required("music", ArgKind::Music)];
static CLEF_PARAMS: &[Param] = &[Param::required("name", ArgKind::String)];
static PROPERTY_PARAMS: &[Param] = &[Param::required("property", ArgKind::PropertyPath)];
static LANGUAGE_PARAMS: &[Param] = &[Param::required("language", ArgKind::String)];
static INCLUDE_PARAMS: &[Param] = &[Param::required("filename", ArgKind::String)];
static KEY_PARAMS: &[Param] = &[
    Param::required("tonic", ArgKind::Pitch),
    Param::required("mode", ArgKind::Word),
];
static TRANSPOSE_PARAMS: &[Param] = &[
    Param::required("from", ArgKind::Pitch),
    Param::required("to", ArgKind::Pitch),
    Param::required("music", ArgKind::Music),
];
static VOLTA_PARAMS: &[Param] = &[
    Param::required("numbers", ArgKind::NumberList),
    Param::required("music", ArgKind::Music),
];
/// The `duration = value` clause inside [`TEMPO_PARAMS`], named separately
/// because a `static`'s initialiser can't hold an inline temporary slice of
/// non-`Copy` [`Param`]s.
static TEMPO_ASSIGNMENT_PARAMS: &[Param] = &[
    Param::required("duration", ArgKind::Count),
    Param::required("=", ArgKind::Literal("=")),
    Param::required("value", ArgKind::Count),
];
/// `\tempo`, in each of its three written forms: `"text"`, `duration =
/// metronome-number`, or both together. The `=` clause is an
/// [`ArgKind::Group`] rather than three independent optional pieces — a bare
/// `\tempo 4 120` (no `=`) must not misread `120` as the metronome number —
/// but is itself optional as a whole, and prefix-preserving while it's being
/// typed: `\tempo 4` alone still parses as far as `duration`. That's enough
/// for [`default_parse`] to walk unaided, so `\tempo` needs no bespoke
/// [`Command`] impl of its own, unlike `\relative`/`\fixed`/`\repeat`/
/// `\new`/`\context`/`\change`.
static TEMPO_PARAMS: &[Param] = &[
    Param::optional("text", ArgKind::String),
    Param::optional("duration = value", ArgKind::Group(TEMPO_ASSIGNMENT_PARAMS)),
];
/// Shared by [`relative`] and [`fixed`], the two commands whose reference
/// pitch is optional and, when present, decides the [`MusicContext`] their
/// body reads in.
static REFERENCE_PITCH_PARAMS: &[Param] = &[
    Param::optional("reference", ArgKind::Pitch),
    Param::required("music", ArgKind::Music),
];

/// A selection of `\clef`'s common clef names, offered at its `name`
/// parameter (index 0). Not exhaustive — LilyPond accepts many more
/// (`varbaritone`, `subbass`, transposed variants with `_8`/`^8`, …) — but
/// these cover what the overwhelming majority of scores actually use.
static CLEF_NAME_CANDIDATES: &[Candidate] = &[
    Candidate::new("treble", "G clef on the second line."),
    Candidate::new("bass", "F clef on the fourth line."),
    Candidate::new("alto", "C clef on the third line."),
    Candidate::new("tenor", "C clef on the fourth line."),
    Candidate::new("percussion", "Neutral clef for unpitched percussion."),
    Candidate::new("treble_8", "Treble clef, sounding an octave lower."),
];
static CLEF_COMPLETIONS: &[&[Candidate]] = &[CLEF_NAME_CANDIDATES];

/// `\key`'s mode word, offered at its `mode` parameter (index 1); index 0
/// (the tonic pitch) is open-ended, so it gets no candidates of its own.
static KEY_MODE_CANDIDATES: &[Candidate] = &[
    Candidate::new("major", "Major (Ionian)."),
    Candidate::new("minor", "Natural minor (Aeolian)."),
    Candidate::new("ionian", "The major scale, named as a church mode."),
    Candidate::new("dorian", "Minor with a raised sixth."),
    Candidate::new("phrygian", "Minor with a lowered second."),
    Candidate::new("lydian", "Major with a raised fourth."),
    Candidate::new("mixolydian", "Major with a lowered seventh."),
    Candidate::new(
        "aeolian",
        "The natural minor scale, named as a church mode.",
    ),
    Candidate::new("locrian", "Diminished-fifth mode, rarely used as a key."),
];
static KEY_COMPLETIONS: &[&[Candidate]] = &[&[], KEY_MODE_CANDIDATES];

/// Curated hover prose for the plain [`StaticCommand`](static_command::StaticCommand)
/// rows that have any worth showing, hoisted out of the tables so each
/// row there stays one line. Most rows say nothing beyond their signature and
/// leave [`Row`]'s doc field `None`.
const ALTERNATIVE_DOC: &str = "Supplies the alternate endings for an enclosing `\\repeat volta` \
     or `\\repeat segno`: one `{ … }` block per ending, in order, inside `music`.";
const VOLTA_DOC: &str = "Marks `music` as belonging to volta (numbered ending) `numbers`, \
     inside an enclosing `\\repeat`.";
const TEMPO_DOC: &str =
    "Sets the tempo. `\\tempo \"Allegro\" 4=120` Either argument may be omitted.";
const CLEF_DOC: &str =
    "Sets the staff's clef to `name` (`treble`, `bass`, `alto`, `tenor`, `percussion`, …).";
const KEY_DOC: &str = "Sets the key signature to `tonic` in `mode` (e.g. `\\major`)";
const TRANSPOSE_DOC: &str = "Transposes `music` so that the pitch written as `from` sounds as \
     `to`, shifting every pitch in `music` by the same interval.";
const NEW_DOC: &str = "Creates a fresh `type` context and interprets `music` in it, even if a \
     context of the same type and name already exists. The optional `name` identifies this \
     instance, for `\\lyricsto`, `\\change` and a later `\\context` to refer back to. May be \
     followed by a `\\with { … }` block.";
const CONTEXT_DOC: &str = "Finds the existing `type` context, optionally the one called `name`, \
     in the enclosing music and interprets `music` in it, creating one only if none is found — unlike `\\new`, \
     which always creates a fresh context. May be followed by a `\\with { … }` block. Inside a \
     `\\layout` block, `\\context { … }` instead defines or modifies a context type rather than \
     instantiating one.";

/// One row of [`RESERVED_ROWS`] or [`CURATED_ROWS`]: a [`StaticCommand`](static_command::StaticCommand)'s
/// data, keyed by one or more names, params, [`MusicContext`], curated doc (if
/// any) and completions (if any), in that order. More than one name is an
/// alias LilyPond itself recognises (`\chordmode`/`\chords`, …); each still
/// gets its own `StaticCommand` instance reporting its own `name`, since two
/// names sharing one instance would make the alias report the other's name
/// back. A plain tuple struct rather than named fields, so a row fits one
/// line and the table reads as a table.
struct Row(
    &'static [&'static str],
    &'static [Param],
    Option<NoteEntry>,
    Option<Region>,
    Option<&'static str>,
    &'static [&'static [Candidate]],
);

/// LilyPond's reserved words: every hand-written command
/// whose only job is to consume a fixed signature and (maybe) set a fixed
/// [`MusicContext`] for its body — `\tempo` included, now that
/// [`ArgKind::Group`] can express its `duration = value` clause as a single
/// optional, prefix-preserving piece of signature rather than a bespoke
/// [`parse_args`](Command::parse_args). `\repeat`, `\relative` and `\fixed`
/// still aren't here — each needs one method [`StaticCommand`](static_command::StaticCommand)
/// can't express, so each gets its own file, wrapping a `StaticCommand` for
/// the rest. `\volta` looks like it should join them too — an earlier
/// version flagged one with no lexically enclosing `\repeat volta`/`\repeat
/// segno` — but that check was unsound: `\volta` is valid inside any
/// `\repeat` kind, in an `\alternative` that follows (rather than sits
/// inside) the repeat, and even in a variable substituted into a `\repeat`
/// body from elsewhere in the file, none of which lexical enclosure can see.
/// So it's a plain row with no validation at all.
///
/// `#[rustfmt::skip]`: rustfmt's default heuristics only hold a call onto one
/// line up to 60% of `max_width`, which would break most of these rows onto
/// five lines apiece — exactly the wall of near-duplicated shape this table
/// exists to avoid. Kept hand-aligned instead, one row per line.
#[rustfmt::skip]
static RESERVED_ROWS: &[Row] = {
    use NoteEntry::Absolute;
    use Region::{ChordMusic, Lyrics, NonNote};
    &[
        Row(&["alternative"],           MUSIC_ONLY_PARAMS,       None,           None,             Some(ALTERNATIVE_DOC), &[]),
        Row(&["notemode", "notes"],     MUSIC_ONLY_PARAMS,       Some(Absolute), None,             None,                  &[]),
        Row(&["chordmode", "chords"],   MUSIC_ONLY_PARAMS,       None,           Some(ChordMusic), None,                  &[]),
        Row(&["drummode", "drums"],     MUSIC_ONLY_PARAMS,       None,           Some(NonNote),    None,                  &[]),
        Row(&["figuremode", "figures"], MUSIC_ONLY_PARAMS,       None,           Some(NonNote),    None,                  &[]),
        Row(&["lyricmode", "lyrics"],   MUSIC_ONLY_PARAMS,       None,           Some(Lyrics),     None,                  &[]),
        Row(&["addlyrics"],             MUSIC_ONLY_PARAMS,       None,           Some(Lyrics),     None,                  &[]),
        Row(&["markup"],                MUSIC_ONLY_PARAMS,       None,           Some(NonNote),    None,                  &[]),
        Row(&["markuplist"],            MUSIC_ONLY_PARAMS,       None,           Some(NonNote),    None,                  &[]),
        Row(&["header"],                MUSIC_ONLY_PARAMS,       None,           Some(NonNote),    None,                  &[]),
        Row(&["paper"],                 MUSIC_ONLY_PARAMS,       None,           Some(NonNote),    None,                  &[]),
        Row(&["layout"],                MUSIC_ONLY_PARAMS,       None,           Some(NonNote),    None,                  &[]),
        Row(&["midi"],                  MUSIC_ONLY_PARAMS,       None,           Some(NonNote),    None,                  &[]),
        Row(&["with"],                  MUSIC_ONLY_PARAMS,       None,           Some(NonNote),    None,                  &[]),
        Row(&["set"],                   PROPERTY_PARAMS,         None,           None,             None,                  &[]),
        Row(&["unset"],                 PROPERTY_PARAMS,         None,           None,             None,                  &[]),
        Row(&["tempo"],                 TEMPO_PARAMS,            None,           None,             Some(TEMPO_DOC),       &[]),
    ]
};

/// The other half of the hand-written table: names LilyPond defines in its own
/// `ly/music-functions-init.ly` as ordinary `define-music-function`s, for which
/// we keep a curated signature and wording because ours is better than what
/// [`scheme`] can read back out of the definition — `\relative` and `\fixed`
/// most of all, whose octave-reference behaviour no signature can express.
///
/// Being ordinary functions, they are *shadowable*: a file that binds `clef`
/// itself means its own, exactly as LilyPond's name lookup does, which is why
/// this layer sits below a document's files rather than above them. That every
/// name here is one the install defines, and no name in [`RESERVED_ROWS`] is,
/// is checked by `install_layer_defines_every_curated_name`.
#[rustfmt::skip]
static CURATED_ROWS: &[Row] = {
    &[
        Row(&["volta"],                 VOLTA_PARAMS,            None,           None,             Some(VOLTA_DOC),       &[]),
        Row(&["clef"],                  CLEF_PARAMS,             None,           None,             Some(CLEF_DOC),        CLEF_COMPLETIONS),
        Row(&["key"],                   KEY_PARAMS,              None,           None,             Some(KEY_DOC),         KEY_COMPLETIONS),
        Row(&["transpose"],             TRANSPOSE_PARAMS,        None,           None,             Some(TRANSPOSE_DOC),   &[]),
    ]
};

/// What hover calls the hand-written layers: this is our own knowledge of
/// LilyPond rather than anything read out of the user's files or install, and
/// saying so is the honest answer to "where does this come from?".
const OURS: &str = "built-in";

/// Builds a [`Layer`] from a row table, keyed by name without the leading
/// backslash. `bespoke` supplies the handful that can't be a plain row: each
/// wraps a [`StaticCommand`](static_command::StaticCommand) and overrides the
/// one method that makes it irregular.
fn table(rows: &[Row], bespoke: Vec<(&str, Arc<dyn Command>)>) -> Layer {
    let mut table: HashMap<String, Arc<dyn Command>> = HashMap::new();

    for Row(names, params, entry, region, doc, completions) in rows {
        for &name in *names {
            table.insert(
                name.to_string(),
                Arc::new(static_command(
                    name,
                    params,
                    *entry,
                    *region,
                    doc.and_then(curated),
                    completions,
                )),
            );
        }
    }

    for (name, command) in bespoke {
        table.insert(name.to_string(), command);
    }

    Layer::new(OURS, table)
}

/// LilyPond's reserved words, which its grammar recognises before any name
/// lookup happens and no file can rebind. Pinned above every other layer by
/// [`Scope::for_document`](crate::vocabulary::Scope::for_document).
pub static RESERVED: LazyLock<Arc<Layer>> = LazyLock::new(|| {
    Arc::new(table(
        RESERVED_ROWS,
        vec![
            ("repeat", Arc::new(repeat::command()) as Arc<dyn Command>),
            ("version", Arc::new(version::command())),
            ("new", Arc::new(new_context::command("new", NEW_DOC))),
            (
                "context",
                Arc::new(new_context::command("context", CONTEXT_DOC)),
            ),
            ("change", Arc::new(change::command())),
            ("lyricsto", Arc::new(lyricsto::command())),
            ("include", Arc::new(language::include(INCLUDE_PARAMS))),
        ],
    ))
});

/// Our curated signatures for ordinary LilyPond music functions — better than
/// the ones [`scheme`] reads out of the install, but shadowable by a file that
/// defines the name itself. See [`CURATED_ROWS`].
pub static CURATED: LazyLock<Arc<Layer>> = LazyLock::new(|| {
    Arc::new(table(
        CURATED_ROWS,
        vec![
            (
                "relative",
                Arc::new(relative::command()) as Arc<dyn Command>,
            ),
            ("fixed", Arc::new(fixed::command())),
            ("language", Arc::new(language::command(LANGUAGE_PARAMS))),
        ],
    ))
});

/// The source-ordered command calls found in a document, queryable by the
/// position or span a refactoring is working at. Mirrors [`Events`] so a call
/// can be located by binary search.
///
/// [`Events`]: crate::notes::Events
#[derive(Debug, Default, Clone)]
pub struct Commands {
    /// In source order by keyword start. A call's body may contain later calls
    /// (a `\volta` inside a `\repeat`), so spans nest rather than stay disjoint.
    calls: Vec<CommandCall>,
}

impl Commands {
    pub fn new(calls: Vec<CommandCall>) -> Self {
        debug_assert!(
            calls
                .windows(2)
                .all(|w| w[0].keyword.start <= w[1].keyword.start),
            "command calls must be in source order by keyword start"
        );
        Self { calls }
    }

    /// The calls whose keyword lies within the half-open byte range `start..end`
    /// — the `\volta`/`\alternative` calls inside a `\repeat` body, say.
    pub fn within(&self, start: usize, end: usize) -> impl Iterator<Item = &CommandCall> {
        self.calls
            .iter()
            .filter(move |call| start <= call.keyword.start && call.keyword.start < end)
    }

    /// The innermost call containing `offset`, and the argument position within
    /// it — the single answer signature help, argument completion and hover
    /// all read from, so each renders it into its own LSP shape rather than
    /// re-deriving "where is the cursor" three times over.
    ///
    /// `src` is the document text. It's needed for one thing beyond what a
    /// [`CommandCall`] already carries: a half-typed call's parsed
    /// [`CommandCall::span`] ends at its last *consumed* argument, so
    /// `\repeat volta ` (nothing typed after the trailing space yet) has a span
    /// ending right after `volta` — but the cursor sitting in that space is
    /// exactly the position signature help most needs to answer. `src` lets
    /// [`covers`] look past the parsed span into that trailing run of
    /// whitespace, up to (not including) whatever real token comes next.
    ///
    /// Calls nest — a `\volta` inside a `\repeat` body is its own entry here,
    /// with a span that sits inside the `\repeat` call's — so "innermost" is
    /// simply the covering call with the latest keyword.
    ///
    /// Returns `None` when `offset` falls strictly inside a call's already-parsed
    /// [`Arg::Music`] body and no nested call covers it there too: plain notes
    /// and events are not an argument position any of the three features has
    /// anything to say about.
    pub fn call_site_at(&self, offset: usize, src: &str) -> Option<CallSite<'_>> {
        let call = self
            .calls
            .iter()
            .filter(|call| covers(call, offset, src))
            .max_by_key(|call| call.keyword.start)?;

        // The parameter the cursor hasn't yet moved past — the one it's
        // inside, or, in the gap before it, the one about to be typed. Past
        // every parsed argument (into the trailing reach `covers` extended us
        // into), this is `None`, and the index falls out at `signature().len()`
        // at most: the next parameter the signature has yet to see.
        let (index, matched) = align_arg_to_param(&call.args, call.cmd.signature(), offset, src);

        if let Some(Arg::Music { span }) = matched
            && span.start < offset
            && offset < span.end
        {
            return None;
        }

        Some(CallSite { call, index })
    }
}

/// Aligns `args` — [`CommandCall::args`], parsed positionally, with any
/// skipped optional parameter simply absent from the vector — against
/// `params`, walking both in lockstep and matching each argument to the next
/// parameter whose [`ArgKind`] it fits, skipping any parameter an argument
/// doesn't match — the same way [`default_parse`] itself decides whether to
/// skip an optional parameter. Returns the parameter index `offset` falls
/// inside or is about to be typed into, together with the argument it
/// landed inside, if any.
///
/// This is what [`Commands::call_site_at`] used to get by reading an
/// argument's *position in `args`* directly as its parameter index — correct
/// only as long as every optional parameter up to that point was actually
/// typed. The moment one is skipped, every later argument sits one slot
/// earlier in `args` than its parameter: `\new type \with { … }`, `[= name]`
/// skipped, parses to `args == [type, with]`, and `with`'s `Arg` sits at
/// `args[1]` — `[= name]`'s own slot in `signature()`, not `with`'s — so
/// reading the index straight off `args` reports `[= name]` active for the
/// rest of the call. Walking `params` in step with `args` and skipping a
/// parameter whenever the next argument doesn't fit its kind keeps the two
/// in line regardless of which optional parameters were actually typed.
///
/// `offset` sitting exactly at an argument's own span end counts as still
/// inside it, generously — `src` is needed for the one case where that's
/// wrong. A bareword, number or fraction has nothing stopping it from
/// growing if another character is typed right there, so the current
/// argument staying active until a separator actually appears is the useful
/// answer. A quoted string is different: its span's last byte is already the
/// closing quote, so typing right past it can never extend the string —
/// [`quoted_string_closes_at_its_own_end`] recognises that shape and treats
/// `offset` landing exactly there as past it instead.
fn align_arg_to_param<'a>(
    args: &'a [Arg],
    params: &[Param],
    offset: usize,
    src: &str,
) -> (usize, Option<&'a Arg>) {
    let mut param_index = 0;
    for arg in args {
        while param_index < params.len() && !arg_matches_kind(arg, &params[param_index].kind) {
            param_index += 1;
        }
        if param_index >= params.len() {
            break;
        }
        let end = arg.span().end;
        let still_inside = if quoted_string_closes_at_its_own_end(arg, src) {
            offset < end
        } else {
            offset <= end
        };
        if still_inside {
            return (param_index, Some(arg));
        }
        param_index += 1;
    }
    (param_index.min(params.len()), None)
}

/// Whether `arg` is an [`Arg::String`] read from a quoted `"…"` — as opposed
/// to the bare-symbol shape [`consume_string`] accepts for the same
/// [`ArgKind::String`] — so that its span's last byte is the closing quote
/// itself. See [`align_arg_to_param`] for why that makes it, uniquely among
/// argument shapes, unable to grow from a character typed right past its own
/// end.
fn quoted_string_closes_at_its_own_end(arg: &Arg, src: &str) -> bool {
    matches!(arg, Arg::String { .. })
        && src.as_bytes().get(arg.span().end.wrapping_sub(1)) == Some(&b'"')
}

/// Whether `arg` is the [`Arg`] variant [`consume_arg`] builds from a node
/// matching `kind` — the correspondence [`align_arg_to_param`] walks
/// [`CommandCall::args`] against `signature()` with. Matches by variant
/// alone (ignoring each side's payload, e.g. an [`ArgKind::Literal`]'s exact
/// text), so it only misaligns two adjacent optional parameters that share
/// the same kind — none of today's signatures do.
fn arg_matches_kind(arg: &Arg, kind: &ArgKind) -> bool {
    matches!(
        (arg, kind),
        (Arg::BareWord { .. }, ArgKind::BareWord)
            | (Arg::Count { .. }, ArgKind::Count)
            | (Arg::NumberList { .. }, ArgKind::NumberList)
            | (Arg::Fraction { .. }, ArgKind::Fraction)
            | (Arg::Music { .. }, ArgKind::Music)
            | (Arg::Pitch { .. }, ArgKind::Pitch)
            | (Arg::Word { .. }, ArgKind::Word)
            | (Arg::String { .. }, ArgKind::String)
            | (Arg::PropertyPath { .. }, ArgKind::PropertyPath)
            | (Arg::Unknown { .. }, ArgKind::Unknown(_))
            | (Arg::Literal { .. }, ArgKind::Literal(_))
            | (Arg::Group { .. }, ArgKind::Group(_))
            | (Arg::ContextType { .. }, ArgKind::ContextType)
            | (Arg::ContextName { .. }, ArgKind::ContextName)
    )
}

/// Where the cursor sits with respect to one [`CommandCall`]: the call itself,
/// and the index into its [`Command::signature`] that the cursor is inside or
/// immediately before. Produced by [`Commands::call_site_at`].
///
/// The index can equal `call.args.len()`, one past the last argument actually
/// parsed: a cursor just past `\repeat volta ` (kind read, nothing else typed
/// yet) reports index 1, the `count` parameter, even though only `kind` is in
/// `call.args`. That's deliberate — it's what makes signature help and
/// completion useful while the argument is still being typed rather than only
/// once it's syntactically complete.
#[derive(Debug, Clone, Copy)]
pub struct CallSite<'a> {
    pub call: &'a CommandCall,
    pub index: usize,
}

/// Whether `call`'s extent — its parsed [`CommandCall::span`], plus any run of
/// whitespace immediately following it that the user may still be typing
/// into — contains `offset`. See [`Commands::call_site_at`] for why the
/// trailing-whitespace reach matters.
///
/// Right at the far end of that reach, where real (non-whitespace) source
/// resumes, `call` covers `offset` only if it still has a parameter left
/// unfilled — `\new Staff` reaching up to a pre-existing `}` right after
/// (nothing typed for `[= name]`/`with`/the required `music` yet) is exactly
/// the "about to type the next piece" case this function exists for: the
/// call is plainly not done, whatever sits next. But `\time 4/4` has matched
/// every parameter in its signature, so extending its reach onto whatever
/// comes next (a note, another call, …) has nothing left for that content to
/// mean — without this check it would linger there anyway, misreporting
/// `\time`'s signature (with no parameter actually active) at the start of
/// unrelated content one call over.
fn covers(call: &CommandCall, offset: usize, src: &str) -> bool {
    if call.span.contains(offset) {
        return true;
    }
    let reach = call.span.end + trailing_whitespace_len(src, call.span.end);
    if !(call.span.end <= offset && offset <= reach) {
        return false;
    }
    if offset < reach || reach == src.len() {
        return true;
    }
    let (index, _) = align_arg_to_param(&call.args, call.cmd.signature(), offset, src);
    index < call.cmd.signature().len()
}

/// The length, in bytes, of the run of ASCII whitespace starting at `from`.
fn trailing_whitespace_len(src: &str, from: usize) -> usize {
    src.as_bytes()[from.min(src.len())..]
        .iter()
        .take_while(|b| b.is_ascii_whitespace())
        .count()
}

impl std::ops::Deref for Commands {
    type Target = [CommandCall];

    fn deref(&self) -> &Self::Target {
        &self.calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note_names::fixture_language;
    use tree_sitter::Tree;

    fn tree(src: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
            .expect("load grammar");
        parser.parse(src, None).expect("parse")
    }

    /// Parses the first command at the top level of `src`, in the default
    /// (Dutch) note-name language. The command may be a bare `escaped_word`
    /// or a `named_context` (`\new`/`\context`) — both are `children[start]`
    /// shapes [`parse`] understands.
    fn call(src: &str) -> Option<CommandCall> {
        let tree = tree(src);
        let root = tree.root_node();
        let mut cursor = root.walk();
        let children: Vec<Node> = root.children(&mut cursor).collect();
        let start = children
            .iter()
            .position(|n| matches!(n.kind(), "escaped_word" | "named_context"))?;
        parse(
            &children,
            start,
            src,
            &fixture_language(),
            &Scope::builtins_only(),
        )
        .map(|(call, _)| call)
    }

    /// [`call`], but also returns the index [`parse`] hands back — the first
    /// unconsumed sibling in the *original* `children` — for the index
    /// arithmetic tests, which care where parsing stopped as much as what it
    /// produced.
    fn call_and_next(src: &str) -> Option<(CommandCall, usize)> {
        let tree = tree(src);
        let root = tree.root_node();
        let mut cursor = root.walk();
        let children: Vec<Node> = root.children(&mut cursor).collect();
        let start = children
            .iter()
            .position(|n| matches!(n.kind(), "escaped_word" | "named_context"))?;
        parse(
            &children,
            start,
            src,
            &fixture_language(),
            &Scope::builtins_only(),
        )
    }

    #[test]
    fn repeat_has_kind_count_and_body() {
        let src = "\\repeat volta 2 { c d }";
        let call = call(src).expect("a repeat call");
        assert_eq!(call.name, "repeat");
        assert!(matches!(&call.args[0], Arg::BareWord { text, .. } if text == "volta"));
        assert!(matches!(call.args[1], Arg::Count { value: 2, .. }));
        assert!(matches!(call.args[2], Arg::Music { .. }));
        // The header stops before the body; the body is the brace block.
        assert_eq!(
            &src[call.header().start..call.header().end],
            "\\repeat volta 2"
        );
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "{ c d }");
    }

    #[test]
    fn unfold_is_a_repeat_kind_too() {
        let call = call("\\repeat unfold 4 { c }").expect("a repeat call");
        assert!(matches!(&call.args[0], Arg::BareWord { text, .. } if text == "unfold"));
        assert!(matches!(call.args[1], Arg::Count { value: 4, .. }));
    }

    #[test]
    fn volta_reads_a_number_list() {
        let call = call("\\volta 1,2,3 { c }").expect("a volta call");
        assert_eq!(call.name, "volta");
        let Arg::NumberList { values, .. } = &call.args[0] else {
            panic!("expected a number list, got {:?}", call.args[0]);
        };
        assert_eq!(values, &[1, 2, 3]);
    }

    #[test]
    fn volta_with_a_single_number() {
        let call = call("\\volta 2 { c }").expect("a volta call");
        assert!(matches!(&call.args[0], Arg::NumberList { values, .. } if values == &[2]));
    }

    #[test]
    fn volta_half_typed_stops_at_the_missing_body() {
        // The required `music` parameter isn't there yet: the number list is
        // read, and consumption stops rather than yielding nothing. Kept as a
        // named regression alongside the general
        // `a_truncated_call_yields_a_prefix_of_the_complete_calls_arguments`
        // property below: unlike the "yields no arguments at all" cases that
        // property subsumed, this documents the specific non-empty prefix
        // `\volta` stops at.
        let call = call("\\volta 1,2").expect("a partial volta call");
        assert_eq!(call.args.len(), 1);
        assert!(call.body().is_none());
    }

    #[test]
    fn alternative_takes_one_music_block() {
        let src = "\\alternative { { a } { b } }";
        let call = call(src).expect("an alternative call");
        assert_eq!(call.name, "alternative");
        assert_eq!(call.args.len(), 1);
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "{ { a } { b } }");
    }

    #[test]
    fn music_can_be_a_braceless_note() {
        // `\repeat percent 4 c2` takes a single note as its music argument, with
        // no surrounding braces; the body spans the whole note including duration.
        let src = "\\repeat percent 4 c2";
        let call = call(src).expect("a repeat call");
        assert!(matches!(call.args[1], Arg::Count { value: 4, .. }));
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "c2");
    }

    #[test]
    fn music_can_be_a_braceless_chord() {
        // A single `<…>` chord with its duration is a braceless music argument too.
        let src = "\\repeat percent 4 <c e>2";
        let call = call(src).expect("a repeat call");
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "<c e>2");
    }

    #[test]
    fn braceless_music_stops_at_the_next_event() {
        // The single note ends where the next, whitespace-separated note begins;
        // the trailing `d8` is left for the caller.
        let src = "{ \\repeat unfold 2 c4 d8 }";
        let tree = tree(src);
        let root = tree.root_node();
        let block = root.child(0).expect("a block");
        let mut cursor = block.walk();
        let children: Vec<Node> = block.children(&mut cursor).collect();
        let start = children
            .iter()
            .position(|n| n.kind() == "escaped_word")
            .unwrap();
        let (call, _) = parse(
            &children,
            start,
            src,
            &fixture_language(),
            &Scope::builtins_only(),
        )
        .expect("a repeat call");
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "c4");
    }

    #[test]
    fn a_missing_argument_stops_consumption() {
        // Half-typed `\repeat volta` with no count or body yet: the kind is read,
        // and the call simply has no further arguments. Kept alongside the
        // property test below, which subsumed the family of "half-typed
        // yields no arguments at all" cases but not this specific non-empty
        // prefix.
        let call = call("\\repeat volta").expect("a partial repeat call");
        assert_eq!(call.args.len(), 1);
        assert!(call.body().is_none());
    }

    #[test]
    fn an_unregistered_command_is_not_parsed() {
        // `\override` has no signature: it takes a property path with an
        // irregular, backtracking shape (`Grob.property = value`, or
        // `Grob.nested.property` for a compound one) that a plain parameter
        // list can't express, so it isn't in the builtin table.
        let tree = tree("\\override NoteHead.color = #red");
        let root = tree.root_node();
        let mut cursor = root.walk();
        let children: Vec<Node> = root.children(&mut cursor).collect();
        assert!(
            parse(
                &children,
                0,
                "\\override NoteHead.color = #red",
                &fixture_language(),
                &Scope::builtins_only(),
            )
            .is_none()
        );
    }

    #[test]
    fn relative_reads_its_reference_pitch() {
        let call = call("\\relative c' { c }").expect("a relative call");
        assert!(matches!(
            call.args[0],
            Arg::Pitch {
                pitch: Pitch {
                    note_name: 0,
                    octave: 0,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(call.args[1], Arg::Music { .. }));
    }

    #[test]
    fn relative_with_no_reference_still_reads_its_body() {
        // The optional pitch parameter fails to match `{`, so it's skipped and
        // the music parameter is tried against the same node.
        let call = call("\\relative { c }").expect("a relative call");
        assert_eq!(call.args.len(), 1);
        assert!(matches!(call.args[0], Arg::Music { .. }));
    }

    #[test]
    fn fixed_reads_its_reference_pitch() {
        let call = call("\\fixed c' { c }").expect("a fixed call");
        assert!(matches!(call.args[0], Arg::Pitch { .. }));
    }

    #[test]
    fn notemode_takes_one_music_block() {
        let call = call("\\notemode { c d }").expect("a notemode call");
        assert_eq!(call.args.len(), 1);
        assert!(matches!(call.args[0], Arg::Music { .. }));
    }

    #[test]
    fn chordmode_takes_one_music_block() {
        let call = call("\\chordmode { c2:maj7 }").expect("a chordmode call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn drummode_takes_one_music_block() {
        let call = call("\\drummode { sn bd }").expect("a drummode call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn figuremode_takes_one_music_block() {
        let call = call("\\figuremode { <6> <5> }").expect("a figuremode call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn lyricmode_takes_one_music_block() {
        let call = call("\\lyricmode { la la }").expect("a lyricmode call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn addlyrics_takes_one_music_block() {
        let call = call("\\addlyrics { la la }").expect("an addlyrics call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn lyricsto_reads_an_optional_voice_name_then_its_body() {
        let call = call("\\lyricsto \"v\" { la }").expect("a lyricsto call");
        assert!(matches!(&call.args[0], Arg::ContextName { name, .. } if name == "v"));
        assert!(matches!(call.args[1], Arg::Music { .. }));
    }

    #[test]
    fn lyricsto_with_no_voice_name_still_reads_its_body() {
        let call = call("\\lyricsto { la }").expect("a lyricsto call");
        assert_eq!(call.args.len(), 1);
        assert!(matches!(call.args[0], Arg::Music { .. }));
    }

    #[test]
    fn markup_takes_one_music_block() {
        let call = call("\\markup { \"x\" }").expect("a markup call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn markuplist_takes_one_music_block() {
        let call = call("\\markuplist { \"x\" }").expect("a markuplist call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn header_takes_one_music_block() {
        let call = call("\\header { title = \"x\" }").expect("a header call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn paper_takes_one_music_block() {
        let call = call("\\paper { }").expect("a paper call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn layout_takes_one_music_block() {
        let call = call("\\layout { }").expect("a layout call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn midi_takes_one_music_block() {
        let call = call("\\midi { }").expect("a midi call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn with_takes_one_music_block() {
        let call = call("\\with { }").expect("a with call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn new_and_context_docs_name_their_own_parameters() {
        // Hover renders these constants directly under the `\new`/`\context`
        // signature, so they earn their keep only if they actually talk
        // about the parameters that signature will show: `type`, `name`
        // (optional) and `music`. Guards the prose against silently
        // drifting from whatever parameter names the later step gives it.
        for doc in [NEW_DOC, CONTEXT_DOC] {
            for param in ["`type`", "`name`", "`music`"] {
                assert!(
                    doc.contains(param),
                    "expected {doc:?} to mention the parameter {param}"
                );
            }
        }
    }

    #[test]
    fn new_and_context_hover_shows_curated_doc_and_signature() {
        let scope = Scope::builtins_only();
        for (name, doc) in [("new", NEW_DOC), ("context", CONTEXT_DOC)] {
            let known = scope
                .get(name)
                .unwrap_or_else(|| panic!("{name} should be reserved"));
            let cmd = known.value;
            let synopsis = cmd
                .synopsis()
                .unwrap_or_else(|| panic!("{name} should have a synopsis"));
            assert!(
                synopsis.contains(&format!("\\{name}")),
                "{synopsis:?} should show the \\{name} signature"
            );
            let documentation = cmd
                .documentation()
                .unwrap_or_else(|| panic!("{name} should have documentation"));
            assert_eq!(documentation.markdown, doc);
        }
    }

    // The six shapes `\new`/`\context` are written in, all exercising
    // `parse`'s `named_context` unwrapping — see `doc/command-parsing.md`
    // and the comment on `parse` itself for why the grammar makes that
    // necessary.

    #[test]
    fn new_reads_a_bare_context_type_and_body() {
        let call = call("\\new Staff { c }").expect("a new call");
        assert_eq!(call.name, "new");
        assert!(matches!(&call.args[0], Arg::ContextType { name, .. } if name == "Staff"));
        assert!(matches!(call.args[1], Arg::Music { .. }));
        assert_eq!(call.args.len(), 2);
    }

    #[test]
    fn context_reads_a_bare_context_type_and_body() {
        let call = call("\\context Staff { c }").expect("a context call");
        assert_eq!(call.name, "context");
        assert!(matches!(&call.args[0], Arg::ContextType { name, .. } if name == "Staff"));
        assert!(matches!(call.args[1], Arg::Music { .. }));
        assert_eq!(call.args.len(), 2);
    }

    #[test]
    fn new_reads_a_named_instance() {
        let call = call("\\new Staff = \"upper\" { c }").expect("a new call");
        assert!(matches!(&call.args[0], Arg::ContextType { name, .. } if name == "Staff"));
        let Arg::Group { args, .. } = &call.args[1] else {
            panic!(
                "expected the = name clause as a Group, got {:?}",
                call.args[1]
            );
        };
        assert!(matches!(args[0], Arg::Literal { .. }));
        assert!(matches!(&args[1], Arg::ContextName { name, .. } if name == "upper"));
        assert!(matches!(call.args[2], Arg::Music { .. }));
        assert_eq!(call.args.len(), 3);
    }

    #[test]
    fn new_reads_a_with_block_before_its_body() {
        let src = "\\new Staff \\with { fontSize = #-2 } { c d e }";
        let call = call(src).expect("a new call");
        assert!(matches!(&call.args[0], Arg::ContextType { name, .. } if name == "Staff"));
        let Arg::Unknown { span } = &call.args[1] else {
            panic!(
                "expected the \\with block as an Unknown arg, got {:?}",
                call.args[1]
            );
        };
        assert_eq!(&src[span.start..span.end], "\\with { fontSize = #-2 }");
        assert!(matches!(call.args[2], Arg::Music { .. }));
        // And the real body — not the `\with` block — is what `body()` finds.
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "{ c d e }");
    }

    #[test]
    fn new_reads_a_named_instance_and_a_with_block() {
        let src = "\\new Voice = \"vocals\" \\with { fontSize = #-2 } { c }";
        let call = call(src).expect("a new call");
        assert!(matches!(&call.args[0], Arg::ContextType { name, .. } if name == "Voice"));
        let Arg::Group { args, .. } = &call.args[1] else {
            panic!(
                "expected the = name clause as a Group, got {:?}",
                call.args[1]
            );
        };
        assert!(matches!(args[0], Arg::Literal { .. }));
        assert!(matches!(&args[1], Arg::ContextName { name, .. } if name == "vocals"));
        assert!(matches!(call.args[2], Arg::Unknown { .. }));
        assert!(matches!(call.args[3], Arg::Music { .. }));
        assert_eq!(call.args.len(), 4);
    }

    #[test]
    fn context_reads_a_named_instance() {
        let call = call("\\context Voice = \"vocals\" { c }").expect("a context call");
        assert_eq!(call.name, "context");
        assert!(matches!(&call.args[0], Arg::ContextType { name, .. } if name == "Voice"));
        let Arg::Group { args, .. } = &call.args[1] else {
            panic!(
                "expected the = name clause as a Group, got {:?}",
                call.args[1]
            );
        };
        assert!(matches!(args[0], Arg::Literal { .. }));
        assert!(matches!(&args[1], Arg::ContextName { name, .. } if name == "vocals"));
        assert!(matches!(call.args[2], Arg::Music { .. }));
        assert_eq!(call.args.len(), 3);
    }

    // The index arithmetic `parse`'s `named_context` arm does to map a
    // position in the flattened stream back to an index into the original
    // `children` — the part of this step most likely to silently re-read or
    // skip a whole music block if it's off by one.

    #[test]
    fn resumes_after_a_bare_context_type_and_body() {
        // Everything the call consumes — the type, entirely inside the
        // `named_context` node, and the body, a sibling after it — is
        // read; the next index must land exactly on the following `\break`.
        let src = "\\new Staff { c } \\break";
        let (call, next) = call_and_next(src).expect("a new call");
        assert_eq!(call.name, "new");
        let tree = tree(src);
        let root = tree.root_node();
        let mut cursor = root.walk();
        let children: Vec<Node> = root.children(&mut cursor).collect();
        assert_eq!(
            &src[children[next].start_byte()..children[next].end_byte()],
            "\\break",
            "expected to resume right at the trailing \\break"
        );
    }

    #[test]
    fn resumes_right_after_a_bare_named_context_node_when_nothing_else_is_consumed() {
        // A half-typed `\new Staff` with nothing else written: everything
        // consumed sits *inside* the `named_context` node, so the next
        // index must be `start + 1` — one past the whole `named_context`
        // node — not some position that only makes sense inside it.
        let src = "\\new Staff";
        let (call, next) = call_and_next(src).expect("a partial new call");
        assert_eq!(call.args.len(), 1);
        assert_eq!(next, 1, "start + 1, one past the sole named_context node");
    }

    #[test]
    fn resumes_after_a_with_block_and_body() {
        let src = "\\new Staff \\with { fontSize = #-2 } { c } \\break";
        let (call, next) = call_and_next(src).expect("a new call");
        assert_eq!(call.args.len(), 3, "type, with block, music");
        let tree = tree(src);
        let root = tree.root_node();
        let mut cursor = root.walk();
        let children: Vec<Node> = root.children(&mut cursor).collect();
        assert_eq!(
            &src[children[next].start_byte()..children[next].end_byte()],
            "\\break",
            "expected to resume right at the trailing \\break, past the \\with block"
        );
    }

    // `\new`/`\context`'s `MusicContext`, which — unlike every plain `Row` —
    // depends on the context type named in the call rather than being fixed
    // by the command.

    #[test]
    fn new_lyrics_reads_as_lyrics() {
        // `Scope::builtins_only()` has no install layer and hence no real
        // `ContextType` for `Lyrics`, so this exercises `context_region`'s
        // fallback: the type name alone is checked against the hand-written
        // root list.
        let call = call("\\new Lyrics { la }").expect("a new call");
        let context = call.cmd.music_context(
            &call,
            MusicContext::new(NoteEntry::Absolute, Region::NoteMusic, fixture_language()),
            &Scope::builtins_only(),
        );
        assert_eq!(context.region, Region::Lyrics);
    }

    #[test]
    fn new_chord_names_still_reads_as_plain_non_note() {
        // The context roots are region-valued now, so the ones that aren't
        // lyrics must not have been swept along with them.
        let call = call("\\new ChordNames { c }").expect("a new call");
        let context = call.cmd.music_context(
            &call,
            MusicContext::new(NoteEntry::Absolute, Region::NoteMusic, fixture_language()),
            &Scope::builtins_only(),
        );
        assert_eq!(context.region, Region::NonNote);
    }

    #[test]
    fn new_staff_reads_as_ordinary_note_music() {
        let call = call("\\new Staff { c }").expect("a new call");
        let context = call.cmd.music_context(
            &call,
            MusicContext::new(NoteEntry::Absolute, Region::NoteMusic, fixture_language()),
            &Scope::builtins_only(),
        );
        assert_eq!(context.region, Region::NoteMusic);
    }

    #[test]
    fn a_users_own_alias_of_lyrics_reads_as_lyrics_too() {
        // `\context { \name MyLyrics \alias Lyrics }` teaches the scope a
        // context type this reader has never heard of by that name; it must
        // still read as lyrics by inheriting from its `\alias`, the same
        // way a real `\new Lyrics` does.
        let context_src = "\\layout { \\context { \\name MyLyrics \\alias Lyrics } }";
        let context_types: HashMap<String, crate::context::ContextType> =
            crate::context::read(&tree(context_src), context_src)
                .into_iter()
                .map(|c| (c.name.clone(), c))
                .collect();
        let layer =
            Arc::new(Layer::new("test.ly", HashMap::new()).with_context_types(context_types));
        let scope = Scope::builtins().for_document(&[layer]);

        let call = call("\\new MyLyrics { la }").expect("a new call");
        let context = call.cmd.music_context(
            &call,
            MusicContext::new(NoteEntry::Absolute, Region::NoteMusic, fixture_language()),
            &scope,
        );
        assert_eq!(context.region, Region::Lyrics);
    }

    #[test]
    fn clef_reads_a_bare_name() {
        let call = call("\\clef bass").expect("a clef call");
        assert!(matches!(&call.args[0], Arg::String { text, .. } if text == "bass"));
    }

    #[test]
    fn clef_reads_a_quoted_name() {
        let call = call("\\clef \"bass\"").expect("a clef call");
        assert!(matches!(&call.args[0], Arg::String { text, .. } if text == "bass"));
    }

    #[test]
    fn set_reads_a_dotted_property_path() {
        let call = call("\\set Staff.instrumentName = \"x\"").expect("a set call");
        let Arg::PropertyPath { path, .. } = &call.args[0] else {
            panic!("expected a property path, got {:?}", call.args[0]);
        };
        assert_eq!(path, &["Staff", "instrumentName"]);
    }

    #[test]
    fn unset_reads_an_undotted_property_path() {
        let call = call("\\unset instrumentName").expect("an unset call");
        let Arg::PropertyPath { path, .. } = &call.args[0] else {
            panic!("expected a property path, got {:?}", call.args[0]);
        };
        assert_eq!(path, &["instrumentName"]);
    }

    #[test]
    fn language_reads_a_bare_name() {
        let call = call("\\language english").expect("a language call");
        assert!(matches!(&call.args[0], Arg::String { text, .. } if text == "english"));
    }

    #[test]
    fn language_reads_a_quoted_name() {
        let call = call("\\language \"english\"").expect("a language call");
        assert!(matches!(&call.args[0], Arg::String { text, .. } if text == "english"));
    }

    #[test]
    fn include_reads_a_quoted_path() {
        let call = call("\\include \"foo.ly\"").expect("an include call");
        assert!(matches!(&call.args[0], Arg::String { text, .. } if text == "foo.ly"));
    }

    #[test]
    fn key_reads_a_tonic_and_a_mode_word() {
        let call = call("\\key g \\major").expect("a key call");
        assert!(matches!(
            call.args[0],
            Arg::Pitch {
                pitch: Pitch { note_name: 4, .. },
                ..
            }
        ));
        assert!(matches!(&call.args[1], Arg::Word { text, .. } if text == "major"));
    }

    #[test]
    fn key_half_typed_stops_at_the_missing_mode() {
        // The tonic is read; the required mode word isn't there yet, so
        // consumption stops rather than guessing. Kept as a named regression
        // for the specific non-empty prefix, alongside the property test
        // below.
        let call = call("\\key g").expect("a partial key call");
        assert_eq!(call.args.len(), 1);
    }

    #[test]
    fn transpose_reads_two_pitches_and_a_body() {
        let call = call("\\transpose c d { e }").expect("a transpose call");
        assert!(matches!(call.args[0], Arg::Pitch { .. }));
        assert!(matches!(call.args[1], Arg::Pitch { .. }));
        assert!(matches!(call.args[2], Arg::Music { .. }));
    }

    #[test]
    fn transpose_half_typed_stops_at_the_missing_body() {
        // Kept as a named regression for this specific non-empty prefix,
        // alongside the property test below.
        let call = call("\\transpose c d").expect("a partial transpose call");
        assert_eq!(call.args.len(), 2);
        assert!(call.body().is_none());
    }

    #[test]
    fn tempo_reads_a_duration_and_metronome_number() {
        let call = call("\\tempo 4 = 120").expect("a tempo call");
        assert_eq!(call.args.len(), 1);
        let Arg::Group { args, .. } = &call.args[0] else {
            panic!(
                "expected the duration/=/value clause as a Group, got {:?}",
                call.args[0]
            );
        };
        assert!(matches!(args[0], Arg::Count { value: 4, .. }));
        assert!(matches!(args[1], Arg::Literal { .. }));
        assert!(matches!(args[2], Arg::Count { value: 120, .. }));
    }

    #[test]
    fn tempo_reads_text_only() {
        let call = call("\\tempo \"Allegro\"").expect("a tempo call");
        assert_eq!(call.args.len(), 1);
        assert!(matches!(&call.args[0], Arg::String { text, .. } if text == "Allegro"));
    }

    #[test]
    fn tempo_reads_text_and_duration_together() {
        let call = call("\\tempo \"Allegro\" 4 = 120").expect("a tempo call");
        assert_eq!(call.args.len(), 2);
        assert!(matches!(&call.args[0], Arg::String { text, .. } if text == "Allegro"));
        let Arg::Group { args, .. } = &call.args[1] else {
            panic!(
                "expected the duration/=/value clause as a Group, got {:?}",
                call.args[1]
            );
        };
        assert!(matches!(args[0], Arg::Count { value: 4, .. }));
        assert!(matches!(args[1], Arg::Literal { .. }));
        assert!(matches!(args[2], Arg::Count { value: 120, .. }));
    }

    #[test]
    fn tempo_duration_without_an_equals_sign_stops_there() {
        // A duration with no `=` isn't a complete metronome mark; the number
        // after it is left unconsumed rather than misread as the value — but
        // the duration itself, prefix-preserving, still comes through as a
        // one-piece Group.
        let call = call("\\tempo 4 120").expect("a tempo call");
        assert_eq!(call.args.len(), 1);
        let Arg::Group { args, .. } = &call.args[0] else {
            panic!("expected a one-piece Group, got {:?}", call.args[0]);
        };
        assert_eq!(args.len(), 1);
        assert!(matches!(args[0], Arg::Count { value: 4, .. }));
    }

    /// The full [`Commands`] for `src`, nested calls and all — what
    /// [`Document::commands`](crate::document::Document::commands) hands out
    /// for a real document, needed here (rather than the single-call [`call`]
    /// helper above) to exercise [`Commands::call_site_at`] against nesting.
    fn commands(src: &str) -> Commands {
        let tree = tree(src);
        crate::note_analyser::analyse(&tree, src, &Scope::builtins_only()).commands
    }

    #[test]
    fn call_site_on_the_keyword_itself() {
        let src = "\\repeat volta 2 { c }";
        let cmds = commands(src);
        let site = cmds.call_site_at(2, src).expect("a call site");
        assert_eq!(site.call.name, "repeat");
        assert_eq!(site.index, 0);
    }

    #[test]
    fn call_site_inside_a_parsed_argument() {
        let src = "\\repeat volta 2 { c }";
        // Offset inside "volta" (the kind argument).
        let offset = src.find("volta").unwrap() + 2;
        let cmds = commands(src);
        let site = cmds.call_site_at(offset, src).expect("a call site");
        assert_eq!(site.call.name, "repeat");
        assert_eq!(site.index, 0);
    }

    #[test]
    fn call_site_in_whitespace_between_arguments() {
        let src = "\\repeat volta  2 { c }";
        // Offset in the gap between "volta" and "2": about to type/edit the
        // next argument, `count` (index 1), not still inside `kind` (index 0).
        let offset = src.find("volta").unwrap() + "volta".len() + 1;
        let cmds = commands(src);
        let site = cmds.call_site_at(offset, src).expect("a call site");
        assert_eq!(site.index, 1);
    }

    #[test]
    fn call_site_just_past_a_half_typed_argument() {
        // Nothing typed yet after "volta " — the trailing whitespace is where
        // signature help and completion are most useful, so the cursor there
        // still resolves to `repeat`, at the next unfilled parameter (`count`,
        // index 1) even though only `kind` has actually been parsed.
        let src = "\\repeat volta ";
        let cmds = commands(src);
        let site = cmds.call_site_at(src.len(), src).expect("a call site");
        assert_eq!(site.call.name, "repeat");
        assert_eq!(site.call.args.len(), 1);
        assert_eq!(site.index, 1);
    }

    #[test]
    fn call_site_right_after_the_bare_keyword() {
        // Nothing typed at all yet: the index is 0, the first parameter.
        let src = "\\repeat ";
        let cmds = commands(src);
        let site = cmds.call_site_at(src.len(), src).expect("a call site");
        assert_eq!(site.call.name, "repeat");
        assert_eq!(site.index, 0);
    }

    #[test]
    fn call_site_prefers_the_innermost_nested_call() {
        let src = "\\repeat volta 2 { \\volta 1 { c } }";
        let offset = src.find("\\volta 1").unwrap() + 2;
        let cmds = commands(src);
        let site = cmds.call_site_at(offset, src).expect("a call site");
        assert_eq!(site.call.name, "volta");
    }

    #[test]
    fn call_site_in_a_nested_calls_trailing_whitespace() {
        // Nothing typed yet after the nested `\volta`: the innermost call
        // wins even while incomplete, not the outer `\repeat` whose own span
        // reaches just as far (it encloses `\volta`'s whole body).
        let src = "\\repeat volta 2 { \\volta }";
        // The whitespace right after "\volta", before the closing brace.
        let offset = src.find(" }").unwrap() + 1;
        let cmds = commands(src);
        let site = cmds.call_site_at(offset, src).expect("a call site");
        assert_eq!(site.call.name, "volta");
        assert_eq!(site.index, 0);
    }

    #[test]
    fn call_site_deep_in_a_music_body_is_none() {
        // Between two plain notes, with no nested command at this position:
        // there is no argument to complete or explain.
        let src = "\\repeat volta 2 { c d e }";
        let offset = src.find(" d ").unwrap() + 1;
        assert!(commands(src).call_site_at(offset, src).is_none());
    }

    #[test]
    fn call_site_at_the_start_of_the_body_still_resolves_to_the_call() {
        // Right at the opening brace — not yet "inside" the body in any
        // meaningful sense — still answers with the outer call's music slot.
        let src = "\\repeat volta 2 { c }";
        let offset = src.find('{').unwrap();
        let cmds = commands(src);
        let site = cmds.call_site_at(offset, src).expect("a call site");
        assert_eq!(site.call.name, "repeat");
        assert_eq!(site.index, 2);
    }

    #[test]
    fn call_site_none_outside_any_call() {
        let src = "c d e";
        assert!(commands(src).call_site_at(1, src).is_none());
    }

    mod proptests {
        use proptest::prelude::*;

        use super::*;

        /// Complete, recognised calls covering every signature shape in
        /// the hand-written tables: a bare word plus count and body (`repeat`), a number
        /// list plus body (`volta`), an optional pitch plus body (`relative`),
        /// a pitch plus word (`key`), two pitches plus body (`transpose`), a
        /// string plus two counts (`tempo`), a bare-word-as-string (`clef`), a
        /// property path (`set`), an optional string plus body (`lyricsto`)
        /// and a bare body (`alternative`, `notemode`).
        const COMPLETE_CALLS: &[&str] = &[
            "\\repeat volta 2 { c }",
            "\\volta 1,2,3 { c }",
            "\\relative c' { c }",
            "\\key g \\major",
            "\\transpose c d { e }",
            "\\tempo \"Allegro\" 4 = 120",
            "\\clef bass",
            "\\set Staff.instrumentName = \"x\"",
            "\\lyricsto \"v\" { la }",
            "\\alternative { { a } { b } }",
            "\\notemode { c d }",
        ];

        /// The `Arg` variant an argument is, ignoring its span and decoded
        /// value — what a truncated call's arguments are compared against the
        /// complete call's on, since truncating necessarily changes spans
        /// (and can change a `NumberList`'s or `Music` block's contents too).
        fn kind(arg: &Arg) -> std::mem::Discriminant<Arg> {
            std::mem::discriminant(arg)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            /// For any complete call and any truncation of it, parsing must
            /// not panic, and the truncated call's arguments — compared kind
            /// by kind — must be a prefix of the complete call's. This is the
            /// real invariant the dozen near-identical `..._half_typed_...`
            /// tests this replaced each sampled one instance of: a half-typed
            /// call stops consuming rather than inventing or misreading an
            /// argument.
            #[test]
            fn a_truncated_call_yields_a_prefix_of_the_complete_calls_arguments(
                index in 0..COMPLETE_CALLS.len(),
                fraction in 0.0f64..=1.0,
            ) {
                let full_src = COMPLETE_CALLS[index];
                let full = call(full_src).expect("the table only holds complete, recognised calls");
                let full_kinds: Vec<_> = full.args.iter().map(kind).collect();

                let cut = (fraction * full_src.len() as f64).round() as usize;
                let truncated_src = &full_src[..cut];
                let truncated = call(truncated_src);
                let truncated_kinds: Vec<_> = truncated
                    .as_ref()
                    .map(|c| c.args.iter().map(kind).collect())
                    .unwrap_or_default();

                prop_assert!(
                    full_kinds.starts_with(&truncated_kinds[..]),
                    "truncating {full_src:?} to {truncated_src:?} gave args of kinds {truncated_kinds:?}, not a prefix of the complete call's {full_kinds:?}",
                );

                // Truncating all the way down to the bare keyword — nothing
                // typed of the arguments yet — yields no arguments at all.
                let keyword_len = full_src.find(' ').unwrap_or(full_src.len());
                let bare = call(&full_src[..keyword_len]);
                let bare_is_empty = bare.as_ref().is_none_or(|c| c.args.is_empty());
                prop_assert!(
                    bare_is_empty,
                    "the bare keyword {:?} yielded arguments",
                    &full_src[..keyword_len],
                );
            }
        }
    }
}
