//! Lexical note-state analysis.
//!
//! LilyPond lets a note omit its duration (inherited from the previous note) and, in `\relative` mode, write only octave *adjustments* rather than an absolute octave. Several refactorings we might add later — extracting music to a variable, adding explicit pitches or durations, converting between relative and absolute entry — need to know each note's *resolved* pitch and duration, i.e. what LilyPond itself would compute reading the music left to right.
//!
//! This module reconstructs that state purely lexically. We never follow a `\foo` reference or an `\include`: the resolved pitch and duration of a note depend only on the notes that lexically precede it. The result is a flat list of [`Event`]s in source order, stored on the `Document` alongside the symbols and includes.
//!
//! The tree-sitter grammar does not group a note's tokens: `cis'8.` is a `symbol` (`cis`), two `punctuation` nodes (`'` and the `.`s), and an `unsigned_integer` (`8`) as flat siblings of the enclosing block. So the analyser walks the children of each music block, recognises these runs, and resolves them against the running state.
//!
//! # What counts as a note
//!
//! At an event position in a music block, any bare `symbol` (not introduced by `\` and not a quoted string) must be either one of the special tokens `r`/`R`/`s`/`q` or a pitch in the [active note-name language](crate::note_names). A `symbol` that is neither is reported as an invalid note (a diagnostic), which surfaces the places this lexical heuristic breaks down.
//!
//! The active language follows `\language "…"` and `\include "….ly"` directives as they appear, starting from LilyPond's default (Dutch). Pitch resolution is only attempted in note mode (the default, `\notemode`, `\relative`, `\fixed`); `\chordmode` gives symbols chord meanings rather than pitches, so its entries are recorded for their extent and duration but not resolved to a pitch (an [`EventKind::ChordModeEvent`]). `\drummode` and `\figuremode` give symbols yet other meanings and are left alone for now.
//!
//! # Known limitations
//!
//! Resolution is best-effort and lexical, so a few constructs are mis-read or skipped, by design for now:
//!
//! - Drum mode (`\drummode`, `\new DrumStaff`, …) is skipped wholesale rather than resolved, so user-defined drum note names need deeper handling later.
//! - A parse error can flatten the tree (e.g. an unformed mode block), after which loose music is read in the wrong mode. Events there are unreliable, but diagnostics falling inside the error region are suppressed so a mid-edit file is not buried in spurious squiggles.
//! - Chord-modifier shorthand in note mode (`c:maj7`, `d:min`) leaves the modifier (`maj`, `min`) looking like a bare symbol, so it is flagged as an invalid note.
//! - A bare note as an unbraced assignment value (`foo = c4`) is not read; only events inside `{ }` / `<< >>` blocks and chords are.
//! - A bare duration (`c4 4`) is read as a note repeating the previous pitch only directly after a single-note event (possibly across a bar check); after a chord or `q`, whose several pitches a single note can't repeat, or as the first token of a block, it is skipped. To tell a real bare duration from a command's integer argument (the `2` of `\repeat … 2`), only an integer following a music event is taken — so a bare duration after a command is missed too.
//! - `\breve`/`\longa` durations (written as words, not numbers) are not recognised, so a following note inherits the previous numeric duration.
//! - Inside nested `{ }` in `\relative` mode, the reference pitch does not propagate back out of the inner block.
//! - Post-events (articulations, fingerings, dynamics, slurs, ties and text or markup scripts) are folded into the event's span but only when attached: a neutral dynamic or articulation set off by whitespace, such as `c4 \f`, is read as a following command instead. A markup post-event written with a chained command (`c-\markup \italic foo`) keeps only the `\markup` in the span; the rest is left loose.

use tree_sitter::{Node, Tree};

use crate::command::{self, Arg, CommandCall, Commands, MusicContext, clamp_octave, is_block};
use crate::line_struct::Span;
use crate::note_names::Language;
use crate::notes::{
    ChordNote, Duration, Event, EventKind, Events, NoteAnalysis, Pitch, Problem, RelativeRef,
};

/// The octave-entry mode in force for a span of music. The default at the top level (and inside a plain `{ }`) is [`Mode::Absolute`].
#[derive(Debug, Clone, Copy)]
enum Mode {
    /// Octave marks are absolute: `c` is `octave -1`, each `'`/`,` adjusts it.
    Absolute,
    /// `\relative`: octave marks adjust from the previous note, whose octave is otherwise the nearest to it. Carries the running reference pitch.
    Relative(Pitch),
    /// `\fixed p`: like absolute, but the bare octave is shifted so an unmarked note sits in `p`'s octave. Carries that octave offset.
    Fixed(i32),
}

/// Whether the children being walked form a note-music event stream, and what mode their nested bare blocks inherit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    /// Bare symbols and chords here are read as note events; nested bare blocks stay note-music.
    NoteMusic,
    /// `\chordmode`: bare symbols here are chord-mode entries (a root with a `:quality`/`/bass`), read for their extent and duration but not their pitch; nested bare blocks stay chord-music.
    ChordMusic,
    /// Not itself an event stream (the top level, a music-function argument), but nested bare blocks are note-music.
    NoteContext,
    /// A non-note region (`\header`, `\lyricmode`, …); symbols are not events and nested bare blocks stay non-note.
    NonNote,
}

impl Region {
    /// The region a nested bare block (one with no governing command) inherits.
    fn nested_block(self) -> Region {
        match self {
            Region::NonNote => Region::NonNote,
            Region::ChordMusic => Region::ChordMusic,
            Region::NoteMusic | Region::NoteContext => Region::NoteMusic,
        }
    }
}

/// The [`MusicContext`] equivalent of the analyser's own (`mode`, `region`)
/// pair at the point a command is encountered — what a [`Command`](command::Command)
/// impl's `music_context` calls `ambient`.
///
/// `Region::NoteContext` (the top level, and any bare block not itself an
/// event stream) collapses into the same case as `NoteMusic`: a command's own
/// body is always a concrete music region once entered, so the distinction
/// only matters for the *bare* blocks [`Region::nested_block`] already
/// handles, never for a command's `ambient`.
fn ambient_context(mode: Mode, region: Region) -> MusicContext {
    match region {
        Region::NonNote => MusicContext::NonNote,
        Region::ChordMusic => MusicContext::Chord,
        Region::NoteMusic | Region::NoteContext => match mode {
            Mode::Absolute => MusicContext::Absolute,
            Mode::Relative(pitch) => MusicContext::Relative(pitch),
            Mode::Fixed(offset) => MusicContext::Fixed(clamp_octave(offset)),
        },
    }
}

/// The inverse of [`ambient_context`]: the (`mode`, `region`) pair to walk a
/// command's `Arg::Music` body in, given the [`MusicContext`] its
/// `music_context` resolved to. `ambient_mode` supplies the mode for
/// [`MusicContext::Chord`] and [`MusicContext::NonNote`], neither of which
/// carries one of its own — chord mode and non-note regions change what a
/// bare symbol means, not the octave-entry mode nested `\relative`/`\fixed`
/// blocks would still reset.
fn mode_and_region(context: MusicContext, ambient_mode: Mode) -> (Mode, Region) {
    match context {
        MusicContext::Inherit => (ambient_mode, Region::NoteMusic),
        MusicContext::Absolute => (Mode::Absolute, Region::NoteMusic),
        MusicContext::Relative(pitch) => (Mode::Relative(pitch), Region::NoteMusic),
        MusicContext::Fixed(offset) => (Mode::Fixed(offset.into()), Region::NoteMusic),
        MusicContext::Chord => (ambient_mode, Region::ChordMusic),
        MusicContext::NonNote => (ambient_mode, Region::NonNote),
    }
}

/// Resolves the note state for every music event in `tree`, in source order.
///
/// Walks music blocks left to right, tracking the octave-entry [`Mode`], the running `\relative` reference pitch, the active note-name language, and the last duration seen.
pub fn analyse(tree: &Tree, src: &str) -> NoteAnalysis {
    let mut analyser = Analyser {
        src,
        events: Vec::new(),
        problems: Vec::new(),
        commands: Vec::new(),
        language: Language::DEFAULT,
        last_duration: Duration::DEFAULT,
        last_pitch: None,
        last_chord: Vec::new(),
    };
    // The top level is not itself a music stream, but its bare blocks are music.
    analyser.walk(tree.root_node(), Mode::Absolute, Region::NoteContext);

    // A file is usually mid-edit, so parse errors are normal. Where the tree is broken the structure can't be trusted — a mode block may not have formed, and its contents then read in the wrong mode — so we drop any diagnostic falling inside an error region rather than bury the real syntax error under a flurry of spurious ones.
    let mut error_spans = Vec::new();
    collect_error_spans(tree.root_node(), &mut error_spans);
    analyser
        .problems
        .retain(|problem| !under_error(problem.span(), &error_spans));

    NoteAnalysis {
        events: Events::new(analyser.events),
        problems: analyser.problems,
        commands: Commands::new(analyser.commands),
    }
}

/// The running state of the left-to-right pass.
struct Analyser<'a> {
    src: &'a str,
    events: Vec<Event>,
    problems: Vec<Problem>,
    /// Structured command invocations recognised by the shared command parser, in source order (preorder, so a `\repeat` precedes the `\volta`s nested in its body).
    commands: Vec<CommandCall>,
    /// Active note-name language; advances on `\language` / language includes.
    language: Language,
    /// Last duration seen anywhere; inherited by an event that omits its own.
    last_duration: Duration,
    /// Pitch of the most recent single note, repeated by a bare duration (the
    /// `4` in `c4 4`). Cleared by a chord or `q`, whose repetition a single
    /// pitch can't represent, and untouched by rests, which carry no pitch.
    last_pitch: Option<Pitch>,
    /// Pitches of the most recent chord, repeated by `q`.
    last_chord: Vec<ChordNote>,
}

impl<'a> Analyser<'a> {
    fn text(&self, node: Node) -> &'a str {
        &self.src[node.start_byte()..node.end_byte()]
    }

    /// True if `node` is a `punctuation` node whose single character is `ch`.
    fn is_punct(&self, node: Node, ch: &str) -> bool {
        node.kind() == "punctuation" && self.text(node) == ch
    }

    /// Walks the children of `parent`. In a [`Region::NoteMusic`] region bare symbols and chords are read as music events resolved against `mode`; otherwise the children are scanned only for nested music and directives.
    fn walk(&mut self, parent: Node, mut mode: Mode, region: Region) {
        let mut cursor = parent.walk();
        let children: Vec<Node> = parent.children(&mut cursor).collect();
        // The region a `\new`/`\context` set for the bare block that follows it.
        let mut pending: Option<Region> = None;
        // Whether the last child read was a music event, so a following bare
        // integer is a bare duration repeating its pitch rather than a command's
        // numeric argument (the `1` of `\volta 1`, the `2` of `\repeat … 2`).
        let mut after_event = false;
        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            match child.kind() {
                "expression_block" | "parallel_music" => {
                    // A bare block inherits the surrounding mode and region, unless a preceding `\new`/`\context` set a region for it.
                    let block_region = pending.take().unwrap_or_else(|| region.nested_block());
                    self.walk(child, mode, block_region);
                    after_event = false;
                    i += 1;
                }
                "chord" if region == Region::NoteMusic => {
                    i = self.read_chord(&children, i, &mut mode);
                    after_event = true;
                }
                "symbol" if region == Region::NoteMusic => {
                    i = self.read_symbol(&children, i, &mut mode);
                    after_event = true;
                }
                // A bare duration with no note name (`c4 4`) repeats the previous
                // note's pitch; read it as a note whose pitch was inherited. Only
                // directly after a music event, so a command's integer argument
                // isn't mistaken for one.
                "unsigned_integer" if region == Region::NoteMusic && after_event => {
                    i = self.read_bare_duration(&children, i, &mut mode);
                    after_event = true;
                }
                "symbol" if region == Region::ChordMusic => {
                    i = self.read_chord_mode_event(&children, i);
                    after_event = false;
                }
                "escaped_word" => {
                    i = self.handle_command(&children, i, mode, region);
                    pending = None;
                    after_event = false;
                }
                // `\new Staff` etc.: the context type decides whether the block that follows is read as note music. `\new Lyrics`/`ChordNames` and friends are not.
                "named_context" => {
                    pending = Some(match self.context_type(child) {
                        Some(kind) if is_non_note_context(kind) => Region::NonNote,
                        _ => region.nested_block(),
                    });
                    after_event = false;
                    i += 1;
                }
                // A bar check between events doesn't break the run, so a bare
                // duration may still follow it (`c4 | 4`).
                "punctuation" if self.is_punct(child, "|") => i += 1,
                _ => {
                    after_event = false;
                    i += 1;
                }
            }
        }
    }

    /// The context type named by a `named_context` node (`Staff` in `\new Staff`), if it has one.
    fn context_type(&self, node: Node) -> Option<&'a str> {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|n| n.kind() == "symbol")
            .map(|n| self.text(n))
    }

    /// Handles an `escaped_word` command at `children[start]`, and returns the
    /// next index to read.
    ///
    /// This is the five steps `doc/command-parsing.md`'s "How the note
    /// analyser uses it" section describes, replacing what used to be a
    /// per-command `match`: look the word up in the shared [`command`] table,
    /// parse its arguments, ask what [`MusicContext`](command::MusicContext)
    /// its body reads in, walk that body's `Music` arguments in it, and
    /// return where to resume. A command with no [`Command`](command::Command)
    /// impl — known, if at all, only by a bare `lilypond-words` entry — is
    /// left entirely to the fallback: this returns `start + 1`, and the loop
    /// in [`walk`](Self::walk) that called us then reaches the following
    /// block itself, through its own `expression_block` arm, exactly as it
    /// does today for `\break`, `\bar`, and everything else with no signature.
    fn handle_command(
        &mut self,
        children: &[Node],
        start: usize,
        mode: Mode,
        region: Region,
    ) -> usize {
        let Some((call, next)) = command::parse(children, start, self.src, self.language) else {
            return start + 1;
        };

        // `\language`/`\include` have a side effect no `Command` method
        // expresses: switching the active note-name language. Rather than add
        // a side-effect method solely for these two, we inspect the parsed
        // call by name here, after the fact — the same string argument the
        // `Command` impl already consumed as an ordinary `Arg::String`.
        if matches!(call.name.as_str(), "language" | "include")
            && let Some(Arg::String { text, .. }) = call.args.first()
        {
            self.set_language(Some(text));
        }

        let ambient = ambient_context(mode, region);
        let context = call.cmd.music_context(&call, ambient);
        let (body_mode, body_region) = mode_and_region(context, mode);
        let music_spans: Vec<Span> = call
            .args
            .iter()
            .filter_map(|arg| match arg {
                Arg::Music { span } => Some(*span),
                _ => None,
            })
            .collect();

        // Recorded before its body is walked, so a call nested in that body
        // (a `\volta` inside a `\repeat`) is pushed after it — `self.commands`
        // is a preorder walk, and `Commands::new` asserts source order by
        // keyword start.
        self.commands.push(call);
        for span in music_spans {
            self.walk_music_arg(children, span, body_mode, body_region);
        }
        next
    }

    /// Walks a command's already-parsed `Arg::Music { span }` in `mode`/`region`.
    /// `span` covers either a `{ … }`/`<< … >>` block — walked like any other
    /// nested block — or a single braceless note or chord (`\repeat percent 4
    /// c2`), which has no container node to hand to [`walk`](Self::walk) and
    /// is instead read directly, the same way the main loop reads one. Locates
    /// the node by its start byte, which `span.start` always matches: the
    /// command parser only ever produces a `Music` argument starting exactly
    /// on the node it consumed.
    fn walk_music_arg(&mut self, children: &[Node], span: Span, mode: Mode, region: Region) {
        let Some(idx) = children.iter().position(|n| n.start_byte() == span.start) else {
            return;
        };
        let node = children[idx];
        if is_block(node.kind()) {
            self.walk(node, mode, region);
            return;
        }
        let mut m = mode;
        match (region, node.kind()) {
            (Region::NoteMusic, "chord") => {
                self.read_chord(children, idx, &mut m);
            }
            (Region::NoteMusic, "symbol") => {
                self.read_symbol(children, idx, &mut m);
            }
            (Region::ChordMusic, "symbol") => {
                self.read_chord_mode_event(children, idx);
            }
            _ => {}
        }
    }

    /// Switches the active note-name language given a `\language`/include name (with or without a `.ly` suffix), if it names a known language.
    fn set_language(&mut self, name: Option<&str>) {
        if let Some(name) = name {
            let name = name.strip_suffix(".ly").unwrap_or(name);
            if let Some(language) = Language::from_name(name) {
                self.language = language;
            }
        }
    }

    /// Reads a note/rest/skip/multi-measure-rest/`q` event whose first token is the symbol at `children[start]`, returning the next index.
    fn read_symbol(&mut self, children: &[Node], start: usize, mode: &mut Mode) -> usize {
        let symbol = children[start];
        let name = self.text(symbol);
        let begin = symbol.start_byte();
        let relative = self.relative_ref(*mode);
        let mut i = start + 1;

        // `r`/`R`/`s`/`q` are never note names; check them before the language.
        let special = match name {
            "r" => Some(EventKind::Rest),
            "R" => Some(EventKind::MultiMeasureRest),
            "s" => Some(EventKind::Skip),
            "q" => Some(EventKind::ChordRepetition(self.repeat_chord(symbol))),
            _ => None,
        };
        if let Some(kind) = special {
            // A chord repetition (`q`) repeats a chord, which a single inherited
            // pitch can't stand in for; a rest or skip carries no pitch. Either
            // way no single pitch is available for a following bare duration.
            if matches!(kind, EventKind::ChordRepetition(_)) {
                self.last_pitch = None;
            }
            let (duration, written) = self.parse_duration(children, &mut i);
            let after_duration = children[i - 1].end_byte();
            let value_end = self.consume_chord_or_tremolo(children, &mut i, after_duration);
            let end = self.consume_post_events(children, &mut i, value_end);
            self.push_event(begin, end, value_end, kind, duration, written, relative);
            return i;
        }

        let Some((note_name, alteration)) = self.language.note(name) else {
            // Not a note name in this language: flag it and move on.
            self.problems.push(Problem::NotANote(Span::new(
                symbol.start_byte(),
                symbol.end_byte(),
            )));
            return start + 1;
        };

        let (marks, octave_written, check) = self.parse_octave(children, &mut i);
        let octave = self.resolve_octave(*mode, note_name, marks, check);
        let pitch = Pitch {
            note_name,
            octave,
            alteration,
        };
        let (duration, written) = self.parse_duration(children, &mut i);
        let after_duration = children[i - 1].end_byte();
        let value_end = self.consume_chord_or_tremolo(children, &mut i, after_duration);
        let end = self.consume_post_events(children, &mut i, value_end);

        if let Mode::Relative(_) = mode {
            *mode = Mode::Relative(pitch);
        }
        self.last_pitch = Some(pitch);
        self.push_event(
            begin,
            end,
            value_end,
            EventKind::Note {
                pitch,
                octave_written: octave_written || check.is_some(),
                pitch_written: true,
            },
            duration,
            written,
            relative,
        );
        i
    }

    /// Reads a bare duration — a duration with no note name, like the `4` in
    /// `c4 4` — which repeats the previous note's pitch. Emits a [`Note`] event
    /// carrying that inherited pitch with `pitch_written` false, or, when no
    /// single pitch is available to inherit (the music opened with this duration,
    /// or the last note was a chord or `q`), consumes the duration without
    /// emitting an event. The first token at `children[start]` is the integer.
    ///
    /// [`Note`]: EventKind::Note
    fn read_bare_duration(&mut self, children: &[Node], start: usize, mode: &mut Mode) -> usize {
        let begin = children[start].start_byte();
        let relative = self.relative_ref(*mode);
        let mut i = start;
        let (duration, written) = self.parse_duration(children, &mut i);
        let after_duration = children[i - 1].end_byte();
        let value_end = self.consume_chord_or_tremolo(children, &mut i, after_duration);
        let end = self.consume_post_events(children, &mut i, value_end);

        let Some(pitch) = self.last_pitch else {
            return i;
        };
        // The bare duration is a full note at the inherited pitch, so it advances
        // the `\relative` reference — to the same pitch, leaving it unchanged.
        if let Mode::Relative(_) = mode {
            *mode = Mode::Relative(pitch);
        }
        self.push_event(
            begin,
            end,
            value_end,
            EventKind::Note {
                pitch,
                octave_written: false,
                pitch_written: false,
            },
            duration,
            written,
            relative,
        );
        i
    }

    /// Reads a chord whose `<…>` node is at `children[start]`, plus the duration that follows it, returning the next index.
    fn read_chord(&mut self, children: &[Node], start: usize, mode: &mut Mode) -> usize {
        let chord = children[start];
        let relative = self.relative_ref(*mode);
        let notes = self.read_chord_notes(chord, *mode);

        // The reference for the next note is the chord's first note.
        if let (Mode::Relative(_), Some(first)) = (*mode, notes.first()) {
            *mode = Mode::Relative(first.pitch);
        }
        if !notes.is_empty() {
            self.last_chord = notes.clone();
        }
        // A following bare duration repeats this whole chord, not a single pitch,
        // so clear the single-note inheritance rather than leave an older note in
        // place for it to wrongly pick up.
        self.last_pitch = None;

        let mut i = start + 1;
        let (duration, written) = self.parse_duration(children, &mut i);
        let after_duration = if i > start + 1 {
            children[i - 1].end_byte()
        } else {
            chord.end_byte()
        };
        let value_end = self.consume_chord_or_tremolo(children, &mut i, after_duration);
        let end = self.consume_post_events(children, &mut i, value_end);
        self.push_event(
            chord.start_byte(),
            end,
            value_end,
            EventKind::Chord(notes),
            duration,
            written,
            relative,
        );
        i
    }

    /// Reads a chord-mode entry whose root symbol is at `children[start]`: the root with its octave marks and duration, then the chord-quality modifier (`:maj7`) and bass/inversion (`/e`) and any post-events, returning the next index. Nothing is resolved — chord mode gives symbols chord meanings, not pitches — so no note-name lookup is done and no diagnostics are raised; the entry is recorded only for its extent and (inherited) duration. [`value_end`](Event::value_end) is left before the `:`/`/`, so an extracted entry that omits its duration has it written as `c4:m`.
    fn read_chord_mode_event(&mut self, children: &[Node], start: usize) -> usize {
        let root = children[start];
        let begin = root.start_byte();
        let mut i = start + 1;
        // Octave marks carry no pitch here, but consuming them keeps them out of
        // the following duration and quality.
        self.parse_octave(children, &mut i);
        let (duration, written) = self.parse_duration(children, &mut i);
        let value_end = children[i - 1].end_byte();
        let qualifier_end = self.consume_chord_qualifiers(children, &mut i, value_end);
        let end = self.consume_post_events(children, &mut i, qualifier_end);
        self.push_event(
            begin,
            end,
            value_end,
            EventKind::ChordModeEvent,
            duration,
            written,
            None,
        );
        i
    }

    /// Consumes the chord-quality modifier (`:maj7`) and bass/inversion (`/e`, `/+e`) that may follow a chord-mode root, together with the run of tokens butting against each `:`/`/`, so they are not mistaken for the entries that follow. Their content is not interpreted. Returns the byte at which the entry now ends.
    fn consume_chord_qualifiers(&self, children: &[Node], i: &mut usize, mut end: usize) -> usize {
        while let Some(intro) = children
            .get(*i)
            .filter(|n| self.is_punct(**n, ":") || self.is_punct(**n, "/"))
        {
            end = intro.end_byte();
            *i += 1;
            while let Some(node) = children.get(*i).filter(|n| n.start_byte() == end) {
                end = node.end_byte();
                *i += 1;
            }
        }
        end
    }

    /// Consumes a trailing `:` and its chord-modifier or tremolo specification (`c:maj7`, `c8:32`), so the modifier tokens are not mistaken for notes. The spec is the run of tokens butting directly against the `:`; a `:` with nothing adjacent after it is a dangling colon and is flagged. Returns the byte at which the event now ends.
    fn consume_chord_or_tremolo(
        &mut self,
        children: &[Node],
        i: &mut usize,
        event_end: usize,
    ) -> usize {
        let Some(colon) = children.get(*i).filter(|n| self.is_punct(**n, ":")) else {
            return event_end;
        };
        *i += 1;
        let mut end = colon.end_byte();
        let mut consumed = false;
        while let Some(node) = children.get(*i).filter(|n| n.start_byte() == end) {
            end = node.end_byte();
            *i += 1;
            consumed = true;
        }
        if !consumed {
            self.problems.push(Problem::DanglingColon(Span::new(
                colon.start_byte(),
                colon.end_byte(),
            )));
            return event_end;
        }
        end
    }

    /// Consumes the post-events attached to the event whose value ends at `value_end` — articulations, fingerings, dynamics, slurs, ties, beams and text or markup scripts — so they count towards the event's span rather than being mistaken for the music that follows. Their content is not interpreted. Returns the byte at which the event ends once they are included.
    fn consume_post_events(&self, children: &[Node], i: &mut usize, value_end: usize) -> usize {
        let mut end = value_end;
        while let Some(&node) = children.get(*i) {
            match node.kind() {
                // A direction indicator (`-`/`^`/`_`) always introduces a script — an articulation, fingering, text or markup — and is invalid alone, so its target is consumed with it.
                "punctuation" if self.is_direction(node) => {
                    *i += 1;
                    end = self.consume_script(children, i, node.end_byte());
                }
                // Slurs, ties and beams attach with no introducer.
                "punctuation" if is_spanner_punct(self.text(node)) => {
                    end = node.end_byte();
                    *i += 1;
                }
                // `\<`/`\>`/`\!`: crescendo, decrescendo and the dynamic stop.
                "dynamic" => {
                    end = node.end_byte();
                    *i += 1;
                }
                // A neutral named articulation or dynamic (`\staccato`, `\f`) carries no direction; we recognise it by its butting directly against the event, so a space-separated command that follows is left for the main loop.
                "escaped_word" if node.start_byte() == end => {
                    end = self.consume_command_post_event(children, i);
                }
                _ => break,
            }
        }
        end
    }

    /// Consumes the script after a direction indicator (`-`/`^`/`_`): a script glyph (`.`, `>`, …), a fingering number, a quoted text string, or a named articulation or markup command. Returns where it ends, or `indicator_end` unchanged for a dangling indicator with nothing after it.
    fn consume_script(&self, children: &[Node], i: &mut usize, indicator_end: usize) -> usize {
        let Some(&node) = children.get(*i) else {
            return indicator_end;
        };
        match node.kind() {
            "escaped_word" => self.consume_command_post_event(children, i),
            "punctuation" | "unsigned_integer" | "string" => {
                *i += 1;
                node.end_byte()
            }
            _ => indicator_end,
        }
    }

    /// Consumes an `escaped_word` acting as a post-event — a named articulation or dynamic (`\trill`, `\f`), or a markup script (`\markup { … }`) — at `children[*i]`, taking a markup command's block, string or Scheme argument with it. Returns its end byte. A markup written with a chained command (`\markup \italic …`) keeps only the `\markup` itself.
    fn consume_command_post_event(&self, children: &[Node], i: &mut usize) -> usize {
        let word = children[*i];
        *i += 1;
        let mut end = word.end_byte();
        if matches!(self.text(word), "\\markup" | "\\markuplist")
            && let Some(&arg) = children.get(*i).filter(|n| is_markup_argument(n.kind()))
        {
            end = arg.end_byte();
            *i += 1;
        }
        end
    }

    /// True if `node` is an articulation direction indicator, `-`, `^` or `_`.
    fn is_direction(&self, node: Node) -> bool {
        node.kind() == "punctuation" && matches!(self.text(node), "-" | "^" | "_")
    }

    /// Resolves the pitches inside a `<…>` chord node. In relative mode the first note is relative to `mode`'s reference and each subsequent note to the one before it.
    fn read_chord_notes(&mut self, chord: Node, mode: Mode) -> Vec<ChordNote> {
        let mut cursor = chord.walk();
        let inner: Vec<Node> = chord.children(&mut cursor).collect();
        let mut notes = Vec::new();
        let mut reference = match mode {
            Mode::Relative(pitch) => Some(pitch),
            _ => None,
        };

        let mut k = 0;
        while k < inner.len() {
            let node = inner[k];
            if node.kind() != "symbol" {
                k += 1;
                continue;
            }
            let begin = node.start_byte();
            let Some((note_name, alteration)) = self.language.note(self.text(node)) else {
                self.problems.push(Problem::NotANote(Span::new(
                    node.start_byte(),
                    node.end_byte(),
                )));
                k += 1;
                continue;
            };
            k += 1;
            let (marks, octave_written, check) = self.parse_octave(&inner, &mut k);
            let octave = match reference {
                Some(pitch) => check.unwrap_or_else(|| relative_octave(pitch, note_name, marks)),
                None => self.resolve_octave(mode, note_name, marks, check),
            };
            let pitch = Pitch {
                note_name,
                octave,
                alteration,
            };
            if reference.is_some() {
                reference = Some(pitch);
            }
            let value_end = inner[k - 1].end_byte();
            let end = self.consume_post_events(&inner, &mut k, value_end);
            notes.push(ChordNote {
                span: Span::new(begin, end),
                pitch,
                octave_written: octave_written || check.is_some(),
            });
        }
        notes
    }

    /// The pitches `q` repeats: the previous chord's notes, re-spanned to `q`.
    fn repeat_chord(&self, q: Node) -> Vec<ChordNote> {
        let span = Span::new(q.start_byte(), q.end_byte());
        self.last_chord
            .iter()
            .map(|note| ChordNote {
                span,
                pitch: note.pitch,
                octave_written: false,
            })
            .collect()
    }

    /// The absolute octave for a note given the mode, written marks, and an optional octave-check override.
    fn resolve_octave(&self, mode: Mode, note_name: u8, marks: i32, check: Option<i32>) -> i32 {
        if let Some(checked) = check {
            return checked;
        }
        match mode {
            Mode::Absolute => marks - 1,
            Mode::Fixed(offset) => marks + offset,
            Mode::Relative(reference) => relative_octave(reference, note_name, marks),
        }
    }

    /// Consumes octave marks (`'`/`,`), accidental reminders (`!`/`?`) and an optional octave check (`='`/`=,`) starting at `children[*i]`. Returns the net octave shift, whether any marks were written, and the checked octave (LilyPond's internal value, `c` = -1) if a check was present.
    fn parse_octave(&self, children: &[Node], i: &mut usize) -> (i32, bool, Option<i32>) {
        let mut marks = 0;
        let mut written = false;
        while let Some(node) = children.get(*i) {
            match self.text(*node) {
                "'" if node.kind() == "punctuation" => {
                    marks += 1;
                    written = true;
                }
                "," if node.kind() == "punctuation" => {
                    marks -= 1;
                    written = true;
                }
                "!" | "?" if node.kind() == "punctuation" => {}
                _ => break,
            }
            *i += 1;
        }

        let mut check = None;
        if children.get(*i).is_some_and(|n| self.is_punct(*n, "=")) {
            *i += 1;
            let mut checked = 0;
            while let Some(node) = children.get(*i) {
                if self.is_punct(*node, "'") {
                    checked += 1;
                } else if self.is_punct(*node, ",") {
                    checked -= 1;
                } else {
                    break;
                }
                *i += 1;
            }
            check = Some(checked - 1);
        }
        (marks, written, check)
    }

    /// Reads an optional duration (number, dots, `*` multipliers) starting at `children[*i]`. Returns the duration in force and whether it was written here; updates the inherited duration when it was.
    fn parse_duration(&mut self, children: &[Node], i: &mut usize) -> (Duration, bool) {
        let Some(number) = children.get(*i).filter(|n| n.kind() == "unsigned_integer") else {
            return (self.last_duration, false);
        };
        let value: u32 = self.text(*number).parse().unwrap_or(4);
        let log = if value == 0 { 2 } else { value.ilog2() as i32 };
        *i += 1;

        let mut dots = 0;
        while children.get(*i).is_some_and(|n| self.is_punct(*n, ".")) {
            dots += 1;
            *i += 1;
        }

        let mut factor = (1u32, 1u32);
        while children.get(*i).is_some_and(|n| self.is_punct(*n, "*")) {
            *i += 1;
            match children.get(*i) {
                Some(n) if n.kind() == "unsigned_integer" => {
                    factor.0 *= self.text(*n).parse().unwrap_or(1);
                    *i += 1;
                }
                Some(n) if n.kind() == "fraction" => {
                    if let Some((num, den)) = self.text(*n).split_once('/') {
                        factor.0 *= num.parse().unwrap_or(1);
                        factor.1 *= den.parse().unwrap_or(1);
                    }
                    *i += 1;
                }
                _ => {}
            }
        }

        let duration = Duration { log, dots, factor };
        self.last_duration = duration;
        (duration, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn push_event(
        &mut self,
        start: usize,
        end: usize,
        value_end: usize,
        kind: EventKind,
        duration: Duration,
        duration_written: bool,
        relative: Option<RelativeRef>,
    ) {
        self.events.push(Event {
            span: Span::new(start, end),
            kind,
            value_end,
            duration,
            duration_written,
            relative,
        });
    }

    /// The `\relative` reference in force in `mode`, spelled in the active
    /// language, or `None` outside `\relative`.
    fn relative_ref(&self, mode: Mode) -> Option<RelativeRef> {
        match mode {
            Mode::Relative(pitch) => Some(RelativeRef {
                pitch,
                text: self.spell_pitch(pitch),
            }),
            _ => None,
        }
    }

    /// Spells `pitch` as absolute LilyPond text (e.g. `cis'`) in the active
    /// language, falling back to the bare note letter for a pitch the language
    /// cannot name.
    fn spell_pitch(&self, pitch: Pitch) -> String {
        const LETTERS: [&str; 7] = ["c", "d", "e", "f", "g", "a", "b"];
        let name = self
            .language
            .spell(pitch.note_name, pitch.alteration)
            .unwrap_or(LETTERS[(pitch.note_name % 7) as usize]);
        format!("{name}{}", absolute_octave_marks(pitch.octave))
    }
}

/// LilyPond's relative-octave rule (`Pitch::to_relative_octave`): place the new note name in whichever octave is closest in diatonic steps to `reference` (ties going down), then apply the net written marks.
pub fn relative_octave(reference: Pitch, note_name: u8, net_marks: i32) -> i32 {
    let here = reference.note_name as i32 + reference.octave * 7;
    let up_octave = reference.octave + i32::from(reference.note_name as i32 > note_name as i32);
    let down_octave = reference.octave - i32::from((reference.note_name as i32) < note_name as i32);
    let up_steps = note_name as i32 + up_octave * 7;
    let down_steps = note_name as i32 + down_octave * 7;
    let chosen = if (up_steps - here).abs() < (down_steps - here).abs() {
        up_octave
    } else {
        down_octave
    };
    chosen + net_marks
}

/// Spells an absolute octave as LilyPond marks: middle C (`octave 0`) is one `'`, the bare `c` (`octave -1`) is none, each step adds a `'` or `,`.
fn absolute_octave_marks(octave: i32) -> String {
    if octave >= 0 {
        "'".repeat((octave + 1) as usize)
    } else {
        ",".repeat((-octave - 1) as usize)
    }
}

/// Punctuation that attaches a slur, tie or beam to a note with no introducing direction: `(`/`)`, `~`, `[`/`]` and the phrasing slurs `\(`/`\)`.
fn is_spanner_punct(text: &str) -> bool {
    matches!(text, "(" | ")" | "~" | "[" | "]" | "\\(" | "\\)")
}

/// Node kinds that can stand as the argument of a `\markup` post-event: a `{ }` block, a quoted string, or embedded Scheme.
fn is_markup_argument(kind: &str) -> bool {
    matches!(kind, "expression_block" | "string" | "embedded_scheme")
}

/// Collects the byte spans of the outermost `ERROR` nodes in the tree, pruning subtrees that contain no error.
fn collect_error_spans(node: Node, out: &mut Vec<Span>) {
    if node.is_error() {
        out.push(Span::new(node.start_byte(), node.end_byte()));
        return;
    }
    if !node.has_error() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_error_spans(child, out);
    }
}

/// Whether `span` lies within any of the error spans.
fn under_error(span: Span, errors: &[Span]) -> bool {
    errors
        .iter()
        .any(|error| error.start <= span.start && span.end <= error.end)
}

/// Whether a `\new`/`\context` context type holds something other than note music, so its bare block should not be read as notes.
fn is_non_note_context(context: &str) -> bool {
    matches!(
        context,
        "Lyrics"
            | "NullVoice"
            | "ChordNames"
            | "FretBoards"
            | "FiguredBass"
            | "Dynamics"
            | "DrumStaff"
            | "DrumVoice"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> NoteAnalysis {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
            .expect("load grammar");
        let tree = parser.parse(src, None).expect("parse");
        analyse(&tree, src)
    }

    /// The resolved pitches of every `Note` event, as `(note_name, octave)`.
    fn pitches(analysis: &NoteAnalysis) -> Vec<(u8, i32)> {
        analysis
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Note { pitch, .. } => Some((pitch.note_name, pitch.octave)),
                _ => None,
            })
            .collect()
    }

    /// The resolved durations of every event, as `(log, dots)`.
    fn durations(analysis: &NoteAnalysis) -> Vec<(i32, u8)> {
        analysis
            .events
            .iter()
            .map(|e| (e.duration.log, e.duration.dots))
            .collect()
    }

    fn not_a_note_count(analysis: &NoteAnalysis) -> usize {
        analysis
            .problems
            .iter()
            .filter(|p| matches!(p, Problem::NotANote(_)))
            .count()
    }

    #[test]
    fn absolute_octaves() {
        // c = -1, c' = 0, c'' = 1, c, = -2.
        let analysis = run("{ c c' c'' c, }");
        assert_eq!(pitches(&analysis), vec![(0, -1), (0, 0), (0, 1), (0, -2)]);
    }

    #[test]
    fn note_names_and_accidentals() {
        let analysis = run("{ cis des' ees }");
        let alterations: Vec<i8> = analysis
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Note { pitch, .. } => Some(pitch.alteration),
                _ => None,
            })
            .collect();
        // cis = c sharp (+2), des = d flat (-2), ees = e flat (-2).
        assert_eq!(pitches(&analysis), vec![(0, -1), (1, 0), (2, -1)]);
        assert_eq!(alterations, vec![2, -2, -2]);
    }

    #[test]
    fn relative_nearest_octave() {
        // The classic: c' then notes chosen as the nearest octave. c' g (down a 4th) c (up a 4th) -> g is below c', c climbs back.
        let analysis = run("\\relative c' { c g c }");
        assert_eq!(pitches(&analysis), vec![(0, 0), (4, -1), (0, 0)]);
    }

    #[test]
    fn relative_with_marks_shifts_octave() {
        // In relative mode each `'` adds an octave on top of the nearest one, so `c''` after c' is two octaves up (octave 2), not absolute c''.
        let analysis = run("\\relative c' { c c'' c }");
        assert_eq!(pitches(&analysis), vec![(0, 0), (0, 2), (0, 2)]);
    }

    #[test]
    fn relative_with_no_reference_defaults_to_middle_c() {
        let analysis = run("\\relative { c e g }");
        // From middle C (c'): e and g are the nearest above.
        assert_eq!(pitches(&analysis), vec![(0, 0), (2, 0), (4, 0)]);
    }

    #[test]
    fn fixed_uses_reference_octave_absolutely() {
        // \fixed c' makes a bare c sit at c' (octave 0), marks absolute from there.
        let analysis = run("\\fixed c' { c c' d, }");
        assert_eq!(pitches(&analysis), vec![(0, 0), (0, 1), (1, -1)]);
    }

    #[test]
    fn duration_is_inherited() {
        let analysis = run("{ c4 d e8 f }");
        // d inherits 4, f inherits 8.
        assert_eq!(durations(&analysis), vec![(2, 0), (2, 0), (3, 0), (3, 0)]);
    }

    #[test]
    fn dots_and_default_duration() {
        let analysis = run("{ c4. d }");
        // The dotted quarter is inherited whole, dots included.
        assert_eq!(durations(&analysis), vec![(2, 1), (2, 1)]);
        assert!(analysis.events[0].duration_written);
        assert!(!analysis.events[1].duration_written);
    }

    #[test]
    fn multiplier_is_part_of_inherited_duration() {
        let analysis = run("{ c4*2/3 d }");
        assert_eq!(analysis.events[0].duration.factor, (2, 3));
        assert_eq!(analysis.events[1].duration.factor, (2, 3));
    }

    #[test]
    fn rest_carries_duration_but_not_pitch() {
        let analysis = run("\\relative c' { c8 r d }");
        // The rest takes 8 and does not advance the relative reference, so d is still resolved from c.
        assert!(matches!(analysis.events[1].kind, EventKind::Rest));
        assert_eq!(pitches(&analysis), vec![(0, 0), (1, 0)]);
        assert_eq!(durations(&analysis), vec![(3, 0), (3, 0), (3, 0)]);
    }

    #[test]
    fn chord_absolute_and_post_chord_reference() {
        let analysis = run("\\relative c' { <c e g> c }");
        let EventKind::Chord(notes) = &analysis.events[0].kind else {
            panic!("expected a chord");
        };
        // First note relative to c'; e and g climb within the chord.
        let resolved: Vec<(u8, i32)> = notes
            .iter()
            .map(|n| (n.pitch.note_name, n.pitch.octave))
            .collect();
        assert_eq!(resolved, vec![(0, 0), (2, 0), (4, 0)]);
        // The note after the chord is relative to its first note (c').
        assert_eq!(pitches(&analysis), vec![(0, 0)]);
    }

    #[test]
    fn chord_repetition_repeats_previous_chord() {
        let analysis = run("{ <c e g>4 q8 }");
        let EventKind::ChordRepetition(notes) = &analysis.events[1].kind else {
            panic!("expected a chord repetition");
        };
        let resolved: Vec<(u8, i32)> = notes
            .iter()
            .map(|n| (n.pitch.note_name, n.pitch.octave))
            .collect();
        assert_eq!(resolved, vec![(0, -1), (2, -1), (4, -1)]);
        // q took its own duration.
        assert_eq!(analysis.events[1].duration.log, 3);
    }

    #[test]
    fn language_switch_changes_note_names() {
        // In English, `cs` is C sharp; in the default Dutch it is not a note.
        let analysis = run("\\language \"english\" { cs ef }");
        assert!(analysis.problems.is_empty());
        let alterations: Vec<i8> = analysis
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Note { pitch, .. } => Some(pitch.alteration),
                _ => None,
            })
            .collect();
        assert_eq!(alterations, vec![2, -2]);
    }

    #[test]
    fn unquoted_language_directive_is_honoured() {
        // `\language english` without quotes must still switch the language.
        let analysis = run("\\language english { ef bf }");
        assert!(analysis.problems.is_empty());
        assert_eq!(pitches(&analysis), vec![(2, -1), (6, -1)]);
    }

    #[test]
    fn command_arguments_are_not_read_as_notes() {
        // The clef name and property path are bare symbols but not notes.
        for src in [
            "{ \\clef bass c }",
            "{ \\set Staff.instrumentName = \"x\" c }",
            "{ \\unset Staff.instrumentName c }",
            "{ \\set melismaBusyProperties = #'() c }",
        ] {
            let analysis = run(src);
            assert!(
                analysis.problems.is_empty(),
                "flagged in {src:?}: {:?}",
                analysis.problems
            );
            assert_eq!(pitches(&analysis), vec![(0, -1)], "in {src:?}");
        }
    }

    #[test]
    fn drum_mode_is_not_read_as_notes() {
        // Drum-mode note names are not flagged because drum mode is skipped.
        let analysis = run("\\drummode { sn8 bd sn bd }");
        assert!(analysis.problems.is_empty());
        assert!(analysis.events.is_empty());
    }

    #[test]
    fn unknown_note_name_is_flagged() {
        // `xyz` is no note in the default language.
        let analysis = run("{ c xyz d }");
        assert_eq!(not_a_note_count(&analysis), 1);
        // The two real notes still resolve.
        assert_eq!(pitches(&analysis), vec![(0, -1), (1, -1)]);
    }

    #[test]
    fn context_name_is_not_a_note() {
        // `Staff` is wrapped in named_context and must not be flagged or read.
        let analysis = run("<< \\new Staff { c } \\new Staff { d } >>");
        assert!(analysis.problems.is_empty());
        assert_eq!(pitches(&analysis), vec![(0, -1), (1, -1)]);
    }

    #[test]
    fn repeat_type_is_not_a_note() {
        let analysis = run("{ \\repeat volta 2 { c d } }");
        assert!(analysis.problems.is_empty());
        assert_eq!(pitches(&analysis), vec![(0, -1), (1, -1)]);
    }

    /// The following tests check that commands with pitch/duration args do not affect how subsequent notes are read.
    fn assert_same_events(with: &str, without: &str) {
        let with_analysis = run(with);
        let without_analysis = run(without);
        assert_eq!(
            pitches(&with_analysis),
            pitches(&without_analysis),
            "pitches differ between {with:?} and {without:?}"
        );
        assert_eq!(
            durations(&with_analysis),
            durations(&without_analysis),
            "durations differ between {with:?} and {without:?}"
        );
    }

    #[test]
    fn key_argument_is_not_read_as_a_note() {
        assert_same_events("{ \\key g \\major a4 b }", "{ a4 b }");
    }

    #[test]
    fn key_argument_does_not_perturb_the_relative_reference() {
        assert_same_events(
            "\\relative c' { \\key g \\major a4 }",
            "\\relative c' { a4 }",
        );
    }

    #[test]
    fn transpose_arguments_are_not_read_as_notes() {
        assert_same_events("{ \\transpose c d { e4 } }", "{ { e4 } }");
    }

    #[test]
    fn transpose_arguments_do_not_perturb_the_relative_reference() {
        assert_same_events(
            "\\relative c' { \\transpose g d { g4 } }",
            "\\relative c' { { g4 } }",
        );
    }

    #[test]
    fn tempo_duration_argument_is_not_the_following_notes_inherited_duration() {
        // `\tempo`'s own `4 = 120` must not become the duration a following bare
        // note inherits; that duration should still come from the last real
        // note.
        assert_same_events("{ c8 \\tempo 4 = 120 d }", "{ c8 d }");
    }

    #[test]
    fn tempo_text_and_duration_argument_does_not_affect_notes() {
        // `\tempo "Allegro" 4 = 120` (both forms combined) — confirmed passing today.
        assert_same_events("{ c8 \\tempo \"Allegro\" 4 = 120 d }", "{ c8 d }");
    }

    #[test]
    fn volta_number_list_argument_is_not_a_duration() {
        let analysis = run("{ \\repeat volta 4 { c8 \\volta 4 { d } } }");
        assert!(analysis.problems.is_empty());
        assert_eq!(durations(&analysis), vec![(3, 0), (3, 0)]);
    }

    #[test]
    fn lyricsto_voice_name_argument_is_not_a_note() {
        // The voice name after \lyricsto (bare symbol form, as opposed to a
        // quoted string) must not be read as a note.
        let analysis = run("\\new Lyrics \\lyricsto v { la la }");
        assert!(analysis.problems.is_empty());
        assert!(analysis.events.is_empty());
    }

    #[test]
    fn notemode_body_matches_the_equivalent_bare_block() {
        // \notemode establishes absolute mode explicitly; at the top level, where
        // a bare block is already absolute, that has no observable effect.
        assert_same_events("{ \\notemode { c d } }", "{ { c d } }");
    }

    #[test]
    fn notes_alias_behaves_like_notemode() {
        assert_same_events("{ \\notes { c d } }", "{ { c d } }");
    }

    #[test]
    fn figuremode_contents_are_not_read_as_notes() {
        let analysis = run("\\figuremode { wobble blah }");
        assert!(analysis.problems.is_empty());
        assert!(analysis.events.is_empty());
    }

    #[test]
    fn figures_alias_behaves_like_figuremode() {
        let analysis = run("\\figures { wobble blah }");
        assert!(analysis.problems.is_empty());
        assert!(analysis.events.is_empty());
    }

    #[test]
    fn lyricmode_contents_are_not_read_as_notes() {
        // The bare `\lyricmode` keyword itself, as opposed to `\new Lyrics { … }`
        // or `\lyricsto`/`\addlyrics`, which the existing `lyrics_are_not_read_as_notes` covers.
        let analysis = run("\\lyricmode { wobble blah }");
        assert!(analysis.problems.is_empty());
        assert!(analysis.events.is_empty());
    }

    #[test]
    fn lyrics_alias_behaves_like_lyricmode() {
        let analysis = run("\\lyrics { wobble blah }");
        assert!(analysis.problems.is_empty());
        assert!(analysis.events.is_empty());
    }

    #[test]
    fn drums_alias_behaves_like_drummode() {
        let analysis = run("\\drums { sn8 bd }");
        assert!(analysis.problems.is_empty());
        assert!(analysis.events.is_empty());
    }

    #[test]
    fn chords_alias_behaves_like_chordmode() {
        let analysis = run("\\chords { c2:maj7 }");
        assert!(analysis.problems.is_empty());
        assert_eq!(analysis.events.len(), 1);
        assert!(matches!(analysis.events[0].kind, EventKind::ChordModeEvent));
    }

    #[test]
    fn with_contents_are_not_read_as_notes() {
        let analysis = run("\\score { { c } \\with { wobble } }");
        assert!(analysis.problems.is_empty());
        assert_eq!(pitches(&analysis), vec![(0, -1)]);
    }

    #[test]
    fn commands_are_recorded_in_source_order_including_nested() {
        // The shared command parser runs in the same pass, so a `\repeat` and the
        // `\volta` nested in its body are both recorded, the outer one first.
        let src = "{ \\repeat volta 2 { c \\volta 1 { d } e } }";
        let analysis = run(src);
        let names: Vec<&str> = analysis.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["repeat", "volta"]);

        // The body of the repeat contains exactly the nested volta call.
        let repeat = &analysis.commands[0];
        let body = repeat.body().expect("repeat has a body");
        let inside: Vec<&str> = analysis
            .commands
            .within(body.start, body.end)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(inside, vec!["volta"]);
        // The notes still resolve normally alongside the recorded commands.
        assert_eq!(pitches(&analysis), vec![(0, -1), (1, -1), (2, -1)]);
    }

    #[test]
    fn chordmode_entry_is_recorded_without_resolving_pitch() {
        // A chord-mode entry is one event spanning the whole `c2:maj7`, with the value ending before the `:maj7` so a duration would read `c4:maj7`. The chord-quality symbols (`maj`) are not flagged as bad notes.
        let src = "\\chordmode { c2:maj7 }";
        let analysis = run(src);
        assert!(analysis.problems.is_empty());
        assert_eq!(analysis.events.len(), 1);
        assert!(matches!(analysis.events[0].kind, EventKind::ChordModeEvent));
        assert_eq!(span_and_value(src, &analysis, 0), ("c2:maj7", "c2"));
    }

    #[test]
    fn chordmode_inversion_is_part_of_the_entry() {
        // The `/e` bass keeps the entry whole, so a selection can't cut between the root and its inversion.
        let src = "\\chordmode { c:m/e d }";
        let analysis = run(src);
        assert!(analysis.problems.is_empty());
        assert_eq!(analysis.events.len(), 2);
        assert_eq!(span_and_value(src, &analysis, 0), ("c:m/e", "c"));
    }

    #[test]
    fn chordmode_propagates_into_nested_blocks() {
        // A bare block nested in chordmode (here a `\repeat` body) stays in chord mode, so its entries are read as chord-mode entries, not notes, and the `:min` modifier is not flagged.
        let analysis = run("\\chordmode { \\repeat unfold 2 { c2:min d } }");
        assert!(analysis.problems.is_empty());
        assert_eq!(analysis.events.len(), 2);
        assert!(
            analysis
                .events
                .iter()
                .all(|e| matches!(e.kind, EventKind::ChordModeEvent))
        );
    }

    #[test]
    fn lyrics_are_not_read_as_notes() {
        // Each lyric mechanism must keep its words out of the event stream.
        for src in [
            "\\new Lyrics { I was lost }",
            "\\new Lyrics \\lyricsto \"v\" { you were found }",
            "{ c d } \\addlyrics { la la }",
        ] {
            let analysis = run(src);
            assert!(
                analysis.problems.is_empty(),
                "lyrics flagged in {src:?}: {:?}",
                analysis.problems
            );
        }
    }

    /// The source text covered by event `n`'s span, and by its note value.
    fn span_and_value<'a>(src: &'a str, analysis: &NoteAnalysis, n: usize) -> (&'a str, &'a str) {
        let event = &analysis.events[n];
        (
            &src[event.span.start..event.span.end],
            &src[event.span.start..event.value_end],
        )
    }

    #[test]
    fn articulation_punctuation_extends_the_span() {
        let src = "{ c4-. d4-> }";
        let analysis = run(src);
        assert!(analysis.problems.is_empty());
        assert_eq!(span_and_value(src, &analysis, 0), ("c4-.", "c4"));
        assert_eq!(span_and_value(src, &analysis, 1), ("d4->", "d4"));
    }

    #[test]
    fn fingering_and_text_scripts_extend_the_span() {
        let src = "{ c-1 d-\"text\" }";
        let analysis = run(src);
        assert!(analysis.problems.is_empty());
        assert_eq!(span_and_value(src, &analysis, 0).0, "c-1");
        assert_eq!(span_and_value(src, &analysis, 1).0, "d-\"text\"");
    }

    #[test]
    fn named_articulations_and_dynamics_extend_the_span() {
        // `\staccato` (with a direction) and the bare dynamics `\f`, `\<`, `\!` all attach to their note, not to the music after it.
        let src = "{ c4-\\staccato d4\\f e\\< f\\! }";
        let analysis = run(src);
        assert!(analysis.problems.is_empty());
        assert_eq!(span_and_value(src, &analysis, 0).0, "c4-\\staccato");
        assert_eq!(span_and_value(src, &analysis, 1).0, "d4\\f");
        assert_eq!(span_and_value(src, &analysis, 2).0, "e\\<");
        assert_eq!(span_and_value(src, &analysis, 3).0, "f\\!");
    }

    #[test]
    fn markup_script_block_is_part_of_the_span() {
        // The markup block must be swallowed whole: its words are not notes, and the span reaches the closing brace.
        let src = "{ c4^\\markup { italic \"x\" } d }";
        let analysis = run(src);
        assert!(
            analysis.problems.is_empty(),
            "markup words flagged: {:?}",
            analysis.problems
        );
        assert_eq!(
            span_and_value(src, &analysis, 0).0,
            "c4^\\markup { italic \"x\" }"
        );
        assert_eq!(pitches(&analysis), vec![(0, -1), (1, -1)]);
    }

    #[test]
    fn slurs_ties_and_beams_attach_to_their_note() {
        let src = "{ c4( d) e~ f[ g] }";
        let analysis = run(src);
        assert!(analysis.problems.is_empty());
        assert_eq!(span_and_value(src, &analysis, 0).0, "c4(");
        assert_eq!(span_and_value(src, &analysis, 1).0, "d)");
        assert_eq!(span_and_value(src, &analysis, 2).0, "e~");
        assert_eq!(span_and_value(src, &analysis, 3).0, "f[");
        assert_eq!(span_and_value(src, &analysis, 4).0, "g]");
    }

    #[test]
    fn space_separated_command_is_not_a_post_event() {
        // A command set off by whitespace belongs to the music, not the note, so
        // the note's span ends at its value.
        let src = "{ c4 \\break d }";
        let analysis = run(src);
        assert!(analysis.problems.is_empty());
        assert_eq!(span_and_value(src, &analysis, 0).0, "c4");
        assert_eq!(pitches(&analysis), vec![(0, -1), (1, -1)]);
    }

    #[test]
    fn rests_carry_their_post_events() {
        let src = "{ r4\\fermata s1\\< }";
        let analysis = run(src);
        assert!(analysis.problems.is_empty());
        assert_eq!(span_and_value(src, &analysis, 0).0, "r4\\fermata");
        assert_eq!(span_and_value(src, &analysis, 1).0, "s1\\<");
    }

    #[test]
    fn chord_inner_notes_carry_their_fingerings() {
        let src = "{ <c-1 e-3>4 }";
        let analysis = run(src);
        assert!(analysis.problems.is_empty());
        let EventKind::Chord(notes) = &analysis.events[0].kind else {
            panic!("expected a chord");
        };
        let spans: Vec<&str> = notes
            .iter()
            .map(|n| &src[n.span.start..n.span.end])
            .collect();
        assert_eq!(spans, vec!["c-1", "e-3"]);
    }

    #[test]
    fn post_events_after_a_chord_extend_its_span() {
        let src = "{ <c e>4\\f-> }";
        let analysis = run(src);
        assert!(analysis.problems.is_empty());
        assert_eq!(span_and_value(src, &analysis, 0), ("<c e>4\\f->", "<c e>4"));
    }

    #[test]
    fn note_mode_tremolo_is_consumed() {
        // `c8:32` is an eighth note played as a 32nd tremolo; the `:32` must not become a separate event, and the note keeps its `8`.
        let analysis = run("{ c8:32 d }");
        assert_eq!(analysis.problems, vec![]);
        assert_eq!(durations(&analysis), vec![(3, 0), (3, 0)]);
        assert_eq!(pitches(&analysis), vec![(0, -1), (1, -1)]);
        // The first event's span covers the whole `c8:32`.
        assert_eq!(
            &"{ c8:32 d }"[analysis.events[0].span.start..analysis.events[0].span.end],
            "c8:32"
        );
    }

    #[test]
    fn chord_modifier_after_chord_is_consumed() {
        let analysis = run("{ <c e>4:maj7 }");
        assert!(analysis.problems.is_empty());
        assert_eq!(analysis.events.len(), 1);
    }

    /// Whether each event wrote its own pitch, or inherited it (a bare duration).
    fn pitch_written(analysis: &NoteAnalysis) -> Vec<bool> {
        analysis
            .events
            .iter()
            .map(|e| {
                !matches!(
                    e.kind,
                    EventKind::Note {
                        pitch_written: false,
                        ..
                    }
                )
            })
            .collect()
    }

    #[test]
    fn bare_duration_repeats_the_previous_pitch() {
        // The `4` and `8` carry no note name, so each repeats the pitch before it.
        let analysis = run("{ c4 4 d8 8 }");
        assert!(analysis.problems.is_empty());
        assert_eq!(pitches(&analysis), vec![(0, -1), (0, -1), (1, -1), (1, -1)]);
        assert_eq!(pitch_written(&analysis), vec![true, false, true, false]);
        // Every bare duration writes its own duration, inheriting nothing.
        assert!(analysis.events.iter().all(|e| e.duration_written));
    }

    #[test]
    fn bare_duration_in_relative_advances_the_reference() {
        // The bare `4` is a c' that advances the reference like any note, so the
        // following `g` is still the nearest g below c' (octave -1), unchanged.
        let analysis = run("\\relative c' { c4 4 g }");
        assert_eq!(pitches(&analysis), vec![(0, 0), (0, 0), (4, -1)]);
    }

    #[test]
    fn bare_duration_needs_a_single_pitch_to_inherit() {
        // A chord or `q` repeats more than one pitch, which a single inherited
        // note can't stand in for, so a bare duration after one is not read.
        for src in ["{ <c e>4 4 }", "{ <c e>4 q4 4 }"] {
            let analysis = run(src);
            assert!(
                analysis.events.iter().all(|e| !matches!(
                    e.kind,
                    EventKind::Note {
                        pitch_written: false,
                        ..
                    }
                )),
                "a bare duration was read in {src:?}"
            );
        }
    }

    #[test]
    fn command_integer_arguments_are_not_bare_durations() {
        // The number arguments of `\repeat`/`\volta` follow a command word, not a
        // music event, so they must not be read as pitch-repeating bare durations.
        let analysis = run("{ c \\repeat volta 2 { d \\volta 1 { e } } }");
        assert!(analysis.problems.is_empty());
        assert_eq!(pitches(&analysis), vec![(0, -1), (1, -1), (2, -1)]);
    }

    #[test]
    fn dangling_colon_is_flagged() {
        let analysis = run("{ c4: d }");
        assert_eq!(analysis.problems.len(), 1);
        assert!(matches!(analysis.problems[0], Problem::DanglingColon(_)));
        // The notes either side still resolve.
        assert_eq!(pitches(&analysis), vec![(0, -1), (1, -1)]);
    }

    #[test]
    fn under_error_detects_containment() {
        // The pure suppression predicate, exercised with fabricated spans so it needs no real ERROR node in the tree.
        let errors = [Span::new(10, 20)];
        assert!(under_error(Span::new(12, 15), &errors)); // strictly inside
        assert!(under_error(Span::new(10, 20), &errors)); // exactly the region
        assert!(!under_error(Span::new(5, 9), &errors)); // before
        assert!(!under_error(Span::new(18, 25), &errors)); // straddles the end
        assert!(!under_error(Span::new(0, 5), &[])); // no errors at all
    }

    #[test]
    fn diagnostics_in_a_broken_region_are_suppressed() {
        // This real drum part trips tree-sitter into wrapping the whole input in an ERROR node, so the `\drummode` block never forms and its `sn`/`bd` would otherwise be read as (invalid) notes. Because they fall inside the error region, every such diagnostic is dropped — leaving only the syntax error the parser already reports.
        let src = "part = \\drummode {\n\
            \x20 << {\n\
            \x20   \\drag sn8-.\\ff sn-. sn-. r \\drag sn8-. sn-. sn-. |\n\
            \x20   \\drag sn4 \\drag sn4 \\drag sn4\n\
            \x20 } \\\\ {\n\
            \x20   bd4 r bd r | bd bd bd\n\
            \x20 } >> r4 |\n\
            \x20 << { sn2:32 sn4 \\drag sn4 | sn } \\\\ { bd4 r r bd | bd } >> r4 r2 |\n";
        let analysis = run(src);
        assert!(
            analysis.problems.is_empty(),
            "expected no diagnostics in the broken region, got {:?}",
            analysis.problems
        );
    }
}
