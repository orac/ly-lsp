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
//! lives behind the [`Command`] trait. [`BUILTIN`] is the hand-written layer —
//! the keyword commands (`\repeat`, `\relative`, `\set`, the mode switches, …)
//! that can only ever be hand-written because they are reserved words in
//! LilyPond's own grammar, not functions a Scheme reader could discover. Most
//! of them are plain rows of [`STATIC_ROWS`], built as a [`StaticCommand`]; the
//! handful with genuinely irregular behaviour — [`relative`], [`fixed`],
//! [`tempo`] and [`repeat`] — each get their own file here, wrapping
//! a `StaticCommand` and overriding the one method that makes them bespoke.
//! [`scheme`] is the other layer built so far: the `define-music-function`s of
//! the user's own files, read into a [`Layer`] per file and stacked into a
//! [`Scope`] by the document graph. See
//! [`doc/command-parsing.md`](../../doc/command-parsing.md) for the fuller
//! design, including the `install` layer still to come.

mod fixed;
mod relative;
mod repeat;
pub mod scheme;
mod static_command;
mod tempo;

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
/// instantiated once per row of [`STATIC_ROWS`]'s table, for every hand-written
/// command whose only job is to consume a fixed signature and (maybe) set a
/// fixed [`MusicContext`] for its body; [`relative::RelativeCommand`],
/// [`fixed::FixedCommand`] and [`tempo::TempoCommand`] are the three whose
/// behaviour genuinely differs. Later a `SchemeCommand` will be instantiated
/// once per `define-music-function` read from the workspace or the install.
///
/// Deliberately object-safe: [`Vocabulary`](crate::vocabulary::Vocabulary)
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
    /// all — `\tempo`'s literal `=` between its duration and its metronome
    /// number is the one hand-written case that needs this. Do not override
    /// merely to reject a bad argument; that belongs in [`check`](Command::check),
    /// so that a wrong-but-parseable call still produces a structured
    /// [`CommandCall`] for the refactorings to work with.
    fn parse_args(&self, args: &mut ArgReader) -> Vec<Arg> {
        default_parse(self.signature(), args)
    }

    /// How the note analyser should read this call's music arguments — the
    /// mode `\relative` establishes, the chord region `\chordmode` establishes,
    /// the non-note region `\lyricmode` establishes.
    ///
    /// Takes the parsed call because the answer often depends on an argument:
    /// `\relative c'` reads its own reference pitch out of `call` and returns
    /// `MusicContext::Relative(pitch)`. Takes `ambient` because some contexts
    /// inherit rather than replace it. The default, returning `ambient`
    /// unchanged, is right for the overwhelming majority of commands, which
    /// neither establish nor block a music context of their own.
    fn music_context(&self, _call: &CommandCall, ambient: MusicContext) -> MusicContext {
        ambient
    }

    /// Hover documentation, already rendered to Markdown. `None` for a command
    /// we recognise but can say nothing about — most of [`STATIC_ROWS`]'s
    /// rows, which say nothing beyond their signature.
    fn documentation(&self) -> Option<&Documentation> {
        None
    }

    /// The values worth completing at parameter `index`. Empty when the
    /// parameter is open-ended, which is most parameters of most commands.
    fn completions(&self, _index: usize) -> &[Candidate] {
        &[]
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
#[derive(Debug, Clone)]
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
}

/// How music inside a command's body is to be read. Mirrors the analyser's
/// internal `Mode`/`Region` pair, which collapses into this once commands stop
/// steering them by hand-written name matches; see `note_analyser`'s
/// `ambient_context`/`mode_and_region` for the two-way conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicContext {
    /// No opinion: read the body the same way the command itself was reached
    /// in. Right for the overwhelming majority of commands (`\repeat`,
    /// `\volta`, `\set`, …), which neither establish nor block a music
    /// context of their own.
    Inherit,
    /// Octave marks are absolute: `c` is `octave -1`.
    Absolute,
    /// `\relative`: octave marks adjust from the previous note, carrying the
    /// running reference pitch.
    Relative(Pitch),
    /// `\fixed p`: an unmarked note sits in `p`'s octave. Carries that octave
    /// offset (`i8` rather than `i32` — the analyser's octave arithmetic keeps
    /// the wider type; this is clamped to it purely for display purposes, and
    /// no real score writes an octave offset anywhere near either bound).
    Fixed(i8),
    /// `\chordmode`: bare symbols are chord-mode entries (a root with a
    /// `:quality`/`/bass`), not pitches.
    Chord,
    /// Lyrics, drums, figures, markup, headers — scanned for nested music and
    /// directives, but bare symbols are not events.
    NonNote,
}

/// A cursor over the sibling nodes following a command keyword, shared by
/// [`default_parse`] and by hand-written overrides so both consume arguments
/// the same way.
pub struct ArgReader<'a> {
    children: &'a [Node<'a>],
    src: &'a str,
    /// The note-name language active where this call was found, needed to
    /// resolve an [`ArgKind::Pitch`] the same way an ordinary note is resolved.
    language: Language,
    /// The index of the first unconsumed sibling.
    next: usize,
}

impl<'a> ArgReader<'a> {
    fn new(children: &'a [Node<'a>], start: usize, src: &'a str, language: Language) -> Self {
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

    /// The index of the first unconsumed sibling — what [`parse`] returns to
    /// its caller.
    pub fn position(&self) -> usize {
        self.next
    }

    /// Consumes a literal punctuation token matching `text` exactly (`=`),
    /// or returns `false` without advancing. Not part of [`ArgKind`] because
    /// it names no argument value — just a fixed separator a bespoke
    /// [`parse_args`](Command::parse_args) needs to step over, as
    /// [`tempo::TempoCommand`] does between its duration and its metronome
    /// number.
    pub fn take_punct(&mut self, text: &str) -> bool {
        match self.children.get(self.next) {
            Some(node) if is_punct(*node, self.src, text) => {
                self.next += 1;
                true
            }
            _ => false,
        }
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

/// Hover text plus where it came from, so the table can prefer our curated
/// wording over LilyPond's own when both exist.
pub struct Documentation {
    /// Markdown, ready for an LSP `MarkupContent`.
    pub markdown: String,
    pub source: DocSource,
}

pub enum DocSource {
    Curated,
    Workspace,
    Install,
}

/// A value worth completing at one parameter position — one of a command's
/// closed set of accepted words, such as `\repeat`'s four kinds or `\key`'s
/// mode names.
///
/// Kept as a plain label/documentation pair rather than an LSP
/// `CompletionItem`: whether the value needs a leading backslash on insertion
/// (an [`ArgKind::Word`] does, a [`ArgKind::BareWord`] doesn't) depends on the
/// parameter it fills, not on the candidate itself, so that decision belongs
/// to the completion feature that renders these, not to `Command` impls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// The word as written in source, without a leading backslash even where
    /// the parameter's [`ArgKind`] is [`Word`](ArgKind::Word).
    pub label: &'static str,
    /// A short, one-line description shown alongside the label.
    pub documentation: &'static str,
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
    /// one [`BUILTIN`] would hand back for `name` today, and later whichever
    /// [`Vocabulary`](crate::vocabulary::Vocabulary) entry resolved it. Carried
    /// here so a caller that already has a `CommandCall` never needs a second
    /// name lookup to ask it anything.
    pub cmd: Arc<dyn Command>,
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
}

impl Arg {
    pub fn span(&self) -> Span {
        match self {
            Arg::BareWord { span, .. }
            | Arg::Count { span, .. }
            | Arg::NumberList { span, .. }
            | Arg::Music { span }
            | Arg::Pitch { span, .. }
            | Arg::Word { span, .. }
            | Arg::String { span, .. }
            | Arg::PropertyPath { span, .. }
            | Arg::Unknown { span } => *span,
        }
    }
}

/// Parses the command at `children[start]` (an `escaped_word`) against the
/// [`Command`] impl `scope` resolves it to, if it resolves to one, consuming
/// as many of its arguments as are present. `language` is the note-name
/// language active at this point in the source, needed to resolve a
/// [`Pitch`](ArgKind::Pitch) argument the same way an ordinary note is
/// resolved.
///
/// Returns the structured call and the index of the first node after the
/// arguments consumed. `None` when the word names no command `scope` has a
/// signature for, leaving the caller to handle it as it did before (in
/// `note_analyser`, that means the following block is read as an ordinary bare
/// block).
pub fn parse(
    children: &[Node],
    start: usize,
    src: &str,
    language: Language,
    scope: &Scope,
) -> Option<(CommandCall, usize)> {
    let keyword_node = *children.get(start)?;
    if keyword_node.kind() != "escaped_word" {
        return None;
    }
    let name = src[keyword_node.start_byte()..keyword_node.end_byte()].strip_prefix('\\')?;
    let cmd = scope.get(name)?.clone();

    let keyword = node_span(keyword_node);
    let mut reader = ArgReader::new(children, start + 1, src, language);
    let args = cmd.parse_args(&mut reader);
    let end = args.last().map_or(keyword.end, |arg| arg.span().end);
    Some((
        CommandCall {
            name: name.to_string(),
            keyword,
            span: Span::new(keyword.start, end),
            args,
            cmd,
        },
        reader.position(),
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
    language: Language,
) -> Option<(Arg, usize)> {
    let node = *children.get(i)?;
    match kind {
        ArgKind::BareWord if node.kind() == "symbol" => {
            let span = node_span(node);
            let text = src[span.start..span.end].to_string();
            Some((Arg::BareWord { span, text }, i + 1))
        }
        ArgKind::Count if node.kind() == "unsigned_integer" => {
            let span = node_span(node);
            let value = src[span.start..span.end].parse().unwrap_or(0);
            Some((Arg::Count { span, value }, i + 1))
        }
        ArgKind::NumberList => consume_number_list(children, i, src),
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
        ArgKind::PropertyPath => consume_property_path(children, i, src),
        ArgKind::Unknown(_) => Some((
            Arg::Unknown {
                span: node_span(node),
            },
            i + 1,
        )),
        _ => None,
    }
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
    language: Language,
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

/// Clamps an octave (the analyser's octave arithmetic is `i32`) to the `i8`
/// [`MusicContext::Fixed`] carries. No real score writes a `\fixed` reference
/// anywhere near either bound; the clamp exists so a pathological one (or a
/// fuzzer) can't panic instead of just misbehaving cosmetically.
pub(crate) fn clamp_octave(octave: i32) -> i8 {
    octave.clamp(i8::MIN as i32, i8::MAX as i32) as i8
}

static MUSIC_ONLY_PARAMS: &[Param] = &[Param::required("music", ArgKind::Music)];
static VOICE_AND_MUSIC_PARAMS: &[Param] = &[
    Param::optional("voice", ArgKind::String),
    Param::required("music", ArgKind::Music),
];
static CLEF_PARAMS: &[Param] = &[Param::required("name", ArgKind::String)];
static PROPERTY_PARAMS: &[Param] = &[Param::required("property", ArgKind::PropertyPath)];
static LANGUAGE_PARAMS: &[Param] = &[Param::required("language", ArgKind::String)];
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
    Candidate {
        label: "treble",
        documentation: "G clef on the second line.",
    },
    Candidate {
        label: "bass",
        documentation: "F clef on the fourth line.",
    },
    Candidate {
        label: "alto",
        documentation: "C clef on the third line.",
    },
    Candidate {
        label: "tenor",
        documentation: "C clef on the fourth line.",
    },
    Candidate {
        label: "percussion",
        documentation: "Neutral clef for unpitched percussion.",
    },
    Candidate {
        label: "treble_8",
        documentation: "Treble clef, sounding an octave lower.",
    },
];
static CLEF_COMPLETIONS: &[&[Candidate]] = &[CLEF_NAME_CANDIDATES];

/// `\key`'s mode word, offered at its `mode` parameter (index 1); index 0
/// (the tonic pitch) is open-ended, so it gets no candidates of its own.
static KEY_MODE_CANDIDATES: &[Candidate] = &[
    Candidate {
        label: "major",
        documentation: "Major (Ionian).",
    },
    Candidate {
        label: "minor",
        documentation: "Natural minor (Aeolian).",
    },
    Candidate {
        label: "ionian",
        documentation: "The major scale, named as a church mode.",
    },
    Candidate {
        label: "dorian",
        documentation: "Minor with a raised sixth.",
    },
    Candidate {
        label: "phrygian",
        documentation: "Minor with a lowered second.",
    },
    Candidate {
        label: "lydian",
        documentation: "Major with a raised fourth.",
    },
    Candidate {
        label: "mixolydian",
        documentation: "Major with a lowered seventh.",
    },
    Candidate {
        label: "aeolian",
        documentation: "The natural minor scale, named as a church mode.",
    },
    Candidate {
        label: "locrian",
        documentation: "Diminished-fifth mode, rarely used as a key.",
    },
];
static KEY_COMPLETIONS: &[&[Candidate]] = &[&[], KEY_MODE_CANDIDATES];

/// Curated hover prose for the plain [`StaticCommand`](static_command::StaticCommand)
/// rows that have any worth showing, hoisted out of [`STATIC_ROWS`] so each
/// row there stays one line. Most rows say nothing beyond their signature and
/// leave [`Row`]'s doc field `None`.
const ALTERNATIVE_DOC: &str = "Supplies the alternate endings for an enclosing `\\repeat volta` \
     or `\\repeat segno`: one `{ … }` block per ending, in order, inside `music`.";
const VOLTA_DOC: &str = "Marks `music` as belonging to volta (numbered ending) `numbers`, \
     inside an enclosing `\\repeat`.";
const CLEF_DOC: &str =
    "Sets the staff's clef to `name` (`treble`, `bass`, `alto`, `tenor`, `percussion`, …).";
const KEY_DOC: &str = "Sets the key signature to `tonic` in `mode` (e.g. `\\major`)";
const TRANSPOSE_DOC: &str = "Transposes `music` so that the pitch written as `from` sounds as \
     `to`, shifting every pitch in `music` by the same interval.";

/// One row of [`STATIC_ROWS`]: a [`StaticCommand`](static_command::StaticCommand)'s
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
    MusicContext,
    Option<&'static str>,
    &'static [&'static [Candidate]],
);

/// The plain data-driven layer of [`BUILTIN`]: every hand-written command
/// whose only job is to consume a fixed signature and (maybe) set a fixed
/// [`MusicContext`] for its body. `\repeat`, `\relative`, `\fixed` and
/// `\tempo` aren't here — each needs one method [`StaticCommand`](static_command::StaticCommand)
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
static STATIC_ROWS: &[Row] = {
    use MusicContext::{Absolute, Chord, Inherit, NonNote};
    &[
        Row(&["alternative"],           MUSIC_ONLY_PARAMS,       Inherit,  Some(ALTERNATIVE_DOC), &[]),
        Row(&["volta"],                 VOLTA_PARAMS,            Inherit,  Some(VOLTA_DOC),       &[]),
        Row(&["notemode", "notes"],     MUSIC_ONLY_PARAMS,       Absolute, None,                  &[]),
        Row(&["chordmode", "chords"],   MUSIC_ONLY_PARAMS,       Chord,    None,                  &[]),
        Row(&["drummode", "drums"],     MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["figuremode", "figures"], MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["lyricmode", "lyrics"],   MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["addlyrics"],             MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["lyricsto"],              VOICE_AND_MUSIC_PARAMS,  NonNote,  None,                  &[]),
        Row(&["markup"],                MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["markuplist"],            MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["header"],                MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["paper"],                 MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["layout"],                MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["midi"],                  MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["with"],                  MUSIC_ONLY_PARAMS,       NonNote,  None,                  &[]),
        Row(&["clef"],                  CLEF_PARAMS,             Inherit,  Some(CLEF_DOC),        CLEF_COMPLETIONS),
        Row(&["set"],                   PROPERTY_PARAMS,         Inherit,  None,                  &[]),
        Row(&["unset"],                 PROPERTY_PARAMS,         Inherit,  None,                  &[]),
        Row(&["language"],              LANGUAGE_PARAMS,         Inherit,  None,                  &[]),
        Row(&["include"],               LANGUAGE_PARAMS,         Inherit,  None,                  &[]),
        Row(&["key"],                   KEY_PARAMS,              Inherit,  Some(KEY_DOC),         KEY_COMPLETIONS),
        Row(&["transpose"],             TRANSPOSE_PARAMS,        Inherit,  Some(TRANSPOSE_DOC),   &[]),
    ]
};

/// Builds the hand-written command table: [`STATIC_ROWS`]' plain rows, plus
/// `\repeat`, `\relative`, `\fixed` and `\tempo`, each of which wraps a
/// [`StaticCommand`](static_command::StaticCommand) and overrides the one
/// method that makes it bespoke. Keyed by name without the leading backslash.
/// Read once into [`BUILTIN`].
fn builtin_table() -> Layer {
    let mut table: HashMap<String, Arc<dyn Command>> = HashMap::new();

    for Row(names, params, context, doc, completions) in STATIC_ROWS {
        for &name in *names {
            table.insert(
                name.to_string(),
                Arc::new(static_command(
                    name,
                    params,
                    *context,
                    doc.and_then(curated),
                    completions,
                )),
            );
        }
    }

    table.insert("repeat".to_string(), Arc::new(repeat::command()));
    table.insert("relative".to_string(), Arc::new(relative::command()));
    table.insert("fixed".to_string(), Arc::new(fixed::command()));
    table.insert("tempo".to_string(), Arc::new(tempo::command()));

    Layer::new(table)
}

/// The hand-written command layer, built once and stacked first by every
/// [`Scope`](crate::vocabulary::Scope). It is a [`Layer`] like any other, so
/// the one way of asking "does ly-lsp know `\foo`?" serves both parsing a call
/// and checking `is_known`, rather than two hand-maintained lists having to
/// agree.
pub(crate) static BUILTIN: LazyLock<Layer> = LazyLock::new(builtin_table);

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

        // The first argument the cursor hasn't yet moved past — the one it's
        // inside, or, in the gap before it, the one about to be typed. Past
        // every parsed argument (into the trailing reach `covers` extended us
        // into), this is `None`, and the index falls out at `args.len()`: the
        // next parameter the signature has yet to see.
        let index = call
            .args
            .iter()
            .position(|arg| offset <= arg.span().end)
            .unwrap_or(call.args.len());

        if let Some(Arg::Music { span }) = call.args.get(index)
            && span.start < offset
            && offset < span.end
        {
            return None;
        }

        Some(CallSite { call, index })
    }
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
fn covers(call: &CommandCall, offset: usize, src: &str) -> bool {
    if call.span.contains(offset) {
        return true;
    }
    let reach = call.span.end + trailing_whitespace_len(src, call.span.end);
    call.span.end <= offset && offset <= reach
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
    use tree_sitter::Tree;

    fn tree(src: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
            .expect("load grammar");
        parser.parse(src, None).expect("parse")
    }

    /// Parses the first command at the top level of `src`, in the default
    /// (Dutch) note-name language.
    fn call(src: &str) -> Option<CommandCall> {
        let tree = tree(src);
        let root = tree.root_node();
        let mut cursor = root.walk();
        let children: Vec<Node> = root.children(&mut cursor).collect();
        let start = children.iter().position(|n| n.kind() == "escaped_word")?;
        parse(
            &children,
            start,
            src,
            Language::DEFAULT,
            &Scope::builtins_only(),
        )
        .map(|(call, _)| call)
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
        // read, and consumption stops rather than yielding nothing.
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
    fn alternative_half_typed_yields_no_arguments() {
        // `\alternative` with nothing after it yet: its one required parameter
        // fails to match, so consumption stops immediately.
        let call = call("\\alternative").expect("a partial alternative call");
        assert!(call.args.is_empty());
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
            Language::DEFAULT,
            &Scope::builtins_only(),
        )
        .expect("a repeat call");
        let body = call.body().expect("a body");
        assert_eq!(&src[body.start..body.end], "c4");
    }

    #[test]
    fn a_missing_argument_stops_consumption() {
        // Half-typed `\repeat volta` with no count or body yet: the kind is read,
        // and the call simply has no further arguments.
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
                Language::DEFAULT,
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
    fn relative_half_typed_yields_no_arguments() {
        let call = call("\\relative").expect("a partial relative call");
        assert!(call.args.is_empty());
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
    fn notemode_half_typed_yields_no_arguments() {
        let call = call("\\notemode").expect("a partial notemode call");
        assert!(call.args.is_empty());
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
        assert!(matches!(&call.args[0], Arg::String { text, .. } if text == "v"));
        assert!(matches!(call.args[1], Arg::Music { .. }));
    }

    #[test]
    fn lyricsto_with_no_voice_name_still_reads_its_body() {
        let call = call("\\lyricsto { la }").expect("a lyricsto call");
        assert_eq!(call.args.len(), 1);
        assert!(matches!(call.args[0], Arg::Music { .. }));
    }

    #[test]
    fn lyricsto_half_typed_yields_no_arguments() {
        let call = call("\\lyricsto").expect("a partial lyricsto call");
        assert!(call.args.is_empty());
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
    fn clef_half_typed_yields_no_arguments() {
        let call = call("\\clef").expect("a partial clef call");
        assert!(call.args.is_empty());
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
    fn set_half_typed_yields_no_arguments() {
        let call = call("\\set").expect("a partial set call");
        assert!(call.args.is_empty());
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
        // consumption stops rather than guessing.
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
        let call = call("\\transpose c d").expect("a partial transpose call");
        assert_eq!(call.args.len(), 2);
        assert!(call.body().is_none());
    }

    #[test]
    fn tempo_reads_a_duration_and_metronome_number() {
        let call = call("\\tempo 4 = 120").expect("a tempo call");
        assert!(matches!(call.args[0], Arg::Count { value: 4, .. }));
        assert!(matches!(call.args[1], Arg::Count { value: 120, .. }));
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
        assert_eq!(call.args.len(), 3);
        assert!(matches!(&call.args[0], Arg::String { text, .. } if text == "Allegro"));
        assert!(matches!(call.args[1], Arg::Count { value: 4, .. }));
        assert!(matches!(call.args[2], Arg::Count { value: 120, .. }));
    }

    #[test]
    fn tempo_duration_without_an_equals_sign_stops_there() {
        // A duration with no `=` isn't a complete metronome mark; the number
        // after it is left unconsumed rather than misread as the value.
        let call = call("\\tempo 4 120").expect("a tempo call");
        assert_eq!(call.args.len(), 1);
        assert!(matches!(call.args[0], Arg::Count { value: 4, .. }));
    }

    #[test]
    fn tempo_half_typed_yields_no_arguments() {
        let call = call("\\tempo").expect("a partial tempo call");
        assert!(call.args.is_empty());
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
}
