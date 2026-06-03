//! Lexical note-state analysis.
//!
//! LilyPond lets a note omit its duration (inherited from the previous note)
//! and, in `\relative` mode, write only octave *adjustments* rather than an
//! absolute octave. Several refactorings we might add later — extracting music
//! to a variable, adding explicit pitches or durations, converting between
//! relative and absolute entry — need to know each note's *resolved* pitch and
//! duration, i.e. what LilyPond itself would compute reading the music left to
//! right.
//!
//! This module reconstructs that state purely lexically. We never follow a
//! `\foo` reference or an `\include`: the resolved pitch and duration of a note
//! depend only on the notes that lexically precede it. The result is a flat
//! list of [`Event`]s in source order, stored on the `Document` alongside the
//! symbols and includes.
//!
//! The tree-sitter grammar does not group a note's tokens: `cis'8.` is a
//! `symbol` (`cis`), two `punctuation` nodes (`'` and the `.`s), and an
//! `unsigned_integer` (`8`) as flat siblings of the enclosing block. So the
//! analyser walks the children of each music block, recognises these runs, and
//! resolves them against the running state.
//!
//! # What counts as a note
//!
//! At an event position in a music block, any bare `symbol` (not introduced by
//! `\` and not a quoted string) must be either one of the special tokens
//! `r`/`R`/`s`/`q` or a pitch in the [active note-name language](crate::note_names).
//! A `symbol` that is neither is reported as an invalid note (a diagnostic),
//! which surfaces the places this lexical heuristic breaks down.
//!
//! The active language follows `\language "…"` and `\include "….ly"` directives
//! as they appear, starting from LilyPond's default (Dutch). Pitch resolution is
//! only attempted in note mode (the default, `\notemode`, `\relative`,
//! `\fixed`); `\chordmode`, `\drummode` and `\figuremode` give symbols different
//! meanings and are left alone for now.
//!
//! # Known limitations
//!
//! Resolution is best-effort and lexical, so a few constructs are mis-read or
//! skipped, by design for now:
//!
//! - Commands whose bare-symbol argument we don't yet skip (`\key c \major`,
//!   `\transpose c d …`) have that pitch read as a spurious note, which also
//!   perturbs the running `\relative` reference. `\clef`, `\set` and `\unset`
//!   arguments are skipped; others may need the same treatment.
//! - Drum mode (`\drummode`, `\new DrumStaff`, …) is skipped wholesale rather
//!   than resolved, so user-defined drum note names need deeper handling later.
//! - A parse error can flatten the tree (e.g. an unformed mode block), after
//!   which loose music is read in the wrong mode. Events there are unreliable,
//!   but diagnostics falling inside the error region are suppressed so a
//!   mid-edit file is not buried in spurious squiggles.
//! - Chord-modifier shorthand in note mode (`c:maj7`, `d:min`) leaves the
//!   modifier (`maj`, `min`) looking like a bare symbol, so it is flagged as an
//!   invalid note.
//! - A bare note as an unbraced assignment value (`foo = c4`) is not read; only
//!   events inside `{ }` / `<< >>` blocks and chords are.
//! - `\breve`/`\longa` durations (written as words, not numbers) are not
//!   recognised, so a following note inherits the previous numeric duration.
//! - Inside nested `{ }` in `\relative` mode, the reference pitch does not
//!   propagate back out of the inner block.

use tree_sitter::{Node, Tree};

use crate::line_struct::Span;
use crate::note_names::Language;
use crate::notes::{ChordNote, Duration, Event, EventKind, Events, NoteAnalysis, Pitch, Problem};

/// The octave-entry mode in force for a span of music. The default at the top
/// level (and inside a plain `{ }`) is [`Mode::Absolute`].
#[derive(Debug, Clone, Copy)]
enum Mode {
    /// Octave marks are absolute: `c` is `octave -1`, each `'`/`,` adjusts it.
    Absolute,
    /// `\relative`: octave marks adjust from the previous note, whose octave is
    /// otherwise the nearest to it. Carries the running reference pitch.
    Relative(Pitch),
    /// `\fixed p`: like absolute, but the bare octave is shifted so an unmarked
    /// note sits in `p`'s octave. Carries that octave offset.
    Fixed(i32),
}

/// Whether the children being walked form a note-music event stream, and what
/// mode their nested bare blocks inherit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    /// Bare symbols and chords here are read as note events; nested bare blocks
    /// stay note-music.
    NoteMusic,
    /// Not itself an event stream (the top level, a music-function argument),
    /// but nested bare blocks are note-music.
    NoteContext,
    /// A non-note region (`\chordmode`, `\header`, …); symbols are not notes and
    /// nested bare blocks stay non-note.
    NonNote,
}

impl Region {
    fn reads_events(self) -> bool {
        matches!(self, Region::NoteMusic)
    }

    /// The region a nested bare block (one with no governing command) inherits.
    fn nested_block(self) -> Region {
        match self {
            Region::NonNote => Region::NonNote,
            Region::NoteMusic | Region::NoteContext => Region::NoteMusic,
        }
    }
}

/// LilyPond's default `\relative` reference when none is written: middle C.
const DEFAULT_RELATIVE_REFERENCE: Pitch = Pitch {
    note_name: 0,
    octave: 0,
    alteration: 0,
};

/// Resolves the note state for every music event in `tree`, in source order.
///
/// Walks music blocks left to right, tracking the octave-entry [`Mode`], the
/// running `\relative` reference pitch, the active note-name language, and the
/// last duration seen.
pub fn analyse(tree: &Tree, src: &str) -> NoteAnalysis {
    let mut analyser = Analyser {
        src,
        events: Vec::new(),
        problems: Vec::new(),
        language: Language::DEFAULT,
        last_duration: Duration::DEFAULT,
        last_chord: Vec::new(),
    };
    // The top level is not itself a music stream, but its bare blocks are music.
    analyser.walk(tree.root_node(), Mode::Absolute, Region::NoteContext);

    // A file is usually mid-edit, so parse errors are normal. Where the tree is
    // broken the structure can't be trusted — a mode block may not have formed,
    // and its contents then read in the wrong mode — so we drop any diagnostic
    // falling inside an error region rather than bury the real syntax error
    // under a flurry of spurious ones.
    let mut error_spans = Vec::new();
    collect_error_spans(tree.root_node(), &mut error_spans);
    analyser
        .problems
        .retain(|problem| !under_error(problem.span(), &error_spans));

    NoteAnalysis {
        events: Events::new(analyser.events),
        problems: analyser.problems,
    }
}

/// The running state of the left-to-right pass.
struct Analyser<'a> {
    src: &'a str,
    events: Vec<Event>,
    problems: Vec<Problem>,
    /// Active note-name language; advances on `\language` / language includes.
    language: Language,
    /// Last duration seen anywhere; inherited by an event that omits its own.
    last_duration: Duration,
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

    /// Walks the children of `parent`. In a [`Region::NoteMusic`] region bare
    /// symbols and chords are read as music events resolved against `mode`;
    /// otherwise the children are scanned only for nested music and directives.
    fn walk(&mut self, parent: Node, mut mode: Mode, region: Region) {
        let read_events = region.reads_events();
        let mut cursor = parent.walk();
        let children: Vec<Node> = parent.children(&mut cursor).collect();
        // The region a `\new`/`\context` set for the bare block that follows it.
        let mut pending: Option<Region> = None;
        let mut i = 0;
        while i < children.len() {
            let child = children[i];
            match child.kind() {
                "expression_block" | "parallel_music" => {
                    // A bare block inherits the surrounding mode and region,
                    // unless a preceding `\new`/`\context` set a region for it.
                    let block_region = pending.take().unwrap_or_else(|| region.nested_block());
                    self.walk(child, mode, block_region);
                    i += 1;
                }
                "chord" if read_events => {
                    i = self.read_chord(&children, i, &mut mode);
                }
                "symbol" if read_events => {
                    i = self.read_symbol(&children, i, &mut mode);
                }
                "escaped_word" => {
                    i = self.handle_command(&children, i, mode);
                    pending = None;
                }
                // `\new Staff` etc.: the context type decides whether the block
                // that follows is read as note music. `\new Lyrics`/`ChordNames`
                // and friends are not.
                "named_context" => {
                    pending = Some(match self.context_type(child) {
                        Some(kind) if is_non_note_context(kind) => Region::NonNote,
                        _ => region.nested_block(),
                    });
                    i += 1;
                }
                _ => i += 1,
            }
        }
    }

    /// The context type named by a `named_context` node (`Staff` in `\new
    /// Staff`), if it has one.
    fn context_type(&self, node: Node) -> Option<&'a str> {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|n| n.kind() == "symbol")
            .map(|n| self.text(n))
    }

    /// Handles an `escaped_word` command at `children[start]`, consuming any
    /// arguments and block it governs, and returns the next index to read.
    fn handle_command(&mut self, children: &[Node], start: usize, ambient: Mode) -> usize {
        let word = self.text(children[start]);
        match word {
            "\\language" | "\\include" => {
                match children.get(start + 1) {
                    Some(node) if node.kind() == "string" => {
                        self.set_language(string_fragment(*node, self.src));
                        start + 2
                    }
                    // `\language english` without quotes: a bare symbol.
                    Some(node) if word == "\\language" && node.kind() == "symbol" => {
                        self.set_language(Some(self.text(*node)));
                        start + 2
                    }
                    _ => start + 1,
                }
            }
            "\\relative" => {
                let mut i = start + 1;
                let reference = self
                    .read_reference_pitch(children, &mut i)
                    .unwrap_or(DEFAULT_RELATIVE_REFERENCE);
                self.enter_block(children, &mut i, Mode::Relative(reference));
                i
            }
            "\\fixed" => {
                let mut i = start + 1;
                let reference = self.read_reference_pitch(children, &mut i);
                let offset = reference.map_or(-1, |p| p.octave);
                self.enter_block(children, &mut i, Mode::Fixed(offset));
                i
            }
            "\\notemode" | "\\notes" => {
                let mut i = start + 1;
                self.enter_block(children, &mut i, Mode::Absolute);
                i
            }
            // Modes where a bare symbol is not a note; scan for nested music but
            // don't read events.
            "\\chordmode" | "\\chords" | "\\drummode" | "\\drums" | "\\figuremode"
            | "\\figures" | "\\lyricmode" | "\\lyrics" | "\\addlyrics" | "\\markup"
            | "\\markuplist" | "\\header" | "\\paper" | "\\layout" | "\\midi" | "\\with" => {
                let mut i = start + 1;
                self.enter_non_event_block(children, &mut i, ambient);
                i
            }
            // `\lyricsto voice { … }`: the lyric block follows a voice name,
            // written as a bare symbol or a quoted string.
            "\\lyricsto" => {
                let mut i = start + 1;
                if matches!(children.get(i).map(|n| n.kind()), Some("string" | "symbol")) {
                    i += 1;
                }
                self.enter_non_event_block(children, &mut i, ambient);
                i
            }
            // The repeat type (`volta`, `unfold`, …) is a bare symbol that must
            // not be read as a note; the count and body follow as normal.
            "\\repeat" => {
                let next = start + 1;
                if children.get(next).map(|n| n.kind()) == Some("symbol") {
                    return next + 1;
                }
                next
            }
            // `\clef bass` takes a single clef name (symbol or string).
            "\\clef" => {
                let next = start + 1;
                if matches!(
                    children.get(next).map(|n| n.kind()),
                    Some("symbol" | "string")
                ) {
                    return next + 1;
                }
                next
            }
            // `\set`/`\unset` take a context-property path (`Staff.instrumentName`)
            // whose names must not be read as notes; any `= value` that follows
            // is left to the main loop.
            "\\set" | "\\unset" => {
                let mut i = start + 1;
                self.skip_property_path(children, &mut i);
                i
            }
            // Any other command: dynamics, articulations, `\break`, etc. Its
            // block argument, if any, is read as music by the main loop.
            _ => start + 1,
        }
    }

    /// Reads an optional reference pitch (a note name with octave marks) used by
    /// `\relative`/`\fixed`, advancing `i` past it. Interpreted as absolute.
    fn read_reference_pitch(&self, children: &[Node], i: &mut usize) -> Option<Pitch> {
        let node = *children.get(*i)?;
        if node.kind() != "symbol" {
            return None;
        }
        let (note_name, alteration) = self.language.note(self.text(node))?;
        *i += 1;
        let (marks, _written, _check) = self.parse_octave(children, i);
        Some(Pitch {
            note_name,
            octave: marks - 1,
            alteration,
        })
    }

    /// If a block follows at `children[*i]`, reads it as note music in `mode`
    /// and advances past it.
    fn enter_block(&mut self, children: &[Node], i: &mut usize, mode: Mode) {
        if let Some(block) = children.get(*i).filter(|n| is_block(n.kind())) {
            self.walk(*block, mode, Region::NoteMusic);
            *i += 1;
        }
    }

    /// Like [`enter_block`](Self::enter_block) but for a non-note context: the
    /// block is scanned for nested music and directives only.
    fn enter_non_event_block(&mut self, children: &[Node], i: &mut usize, ambient: Mode) {
        if let Some(block) = children.get(*i).filter(|n| is_block(n.kind())) {
            self.walk(*block, ambient, Region::NonNote);
            *i += 1;
        }
    }

    /// Skips a context-property path such as `Staff.instrumentName` — symbols
    /// joined by dots — so the property names are not read as notes.
    fn skip_property_path(&self, children: &[Node], i: &mut usize) {
        while children.get(*i).map(|n| n.kind()) == Some("symbol") {
            *i += 1;
            if children.get(*i).is_some_and(|n| self.is_punct(*n, ".")) {
                *i += 1;
            } else {
                break;
            }
        }
    }

    /// Switches the active note-name language given a `\language`/include name
    /// (with or without a `.ly` suffix), if it names a known language.
    fn set_language(&mut self, name: Option<&str>) {
        if let Some(name) = name {
            let name = name.strip_suffix(".ly").unwrap_or(name);
            if let Some(language) = Language::from_name(name) {
                self.language = language;
            }
        }
    }

    /// Reads a note/rest/skip/multi-measure-rest/`q` event whose first token is
    /// the symbol at `children[start]`, returning the next index.
    fn read_symbol(&mut self, children: &[Node], start: usize, mode: &mut Mode) -> usize {
        let symbol = children[start];
        let name = self.text(symbol);
        let begin = symbol.start_byte();
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
            let (duration, written) = self.parse_duration(children, &mut i);
            let after_duration = children[i - 1].end_byte();
            let end = self.consume_chord_or_tremolo(children, &mut i, after_duration);
            self.push_event(begin, end, kind, duration, written);
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
        let end = self.consume_chord_or_tremolo(children, &mut i, after_duration);

        if let Mode::Relative(_) = mode {
            *mode = Mode::Relative(pitch);
        }
        self.push_event(
            begin,
            end,
            EventKind::Note {
                pitch,
                octave_written: octave_written || check.is_some(),
            },
            duration,
            written,
        );
        i
    }

    /// Reads a chord whose `<…>` node is at `children[start]`, plus the duration
    /// that follows it, returning the next index.
    fn read_chord(&mut self, children: &[Node], start: usize, mode: &mut Mode) -> usize {
        let chord = children[start];
        let notes = self.read_chord_notes(chord, *mode);

        // The reference for the next note is the chord's first note.
        if let (Mode::Relative(_), Some(first)) = (*mode, notes.first()) {
            *mode = Mode::Relative(first.pitch);
        }
        if !notes.is_empty() {
            self.last_chord = notes.clone();
        }

        let mut i = start + 1;
        let (duration, written) = self.parse_duration(children, &mut i);
        let after_duration = if i > start + 1 {
            children[i - 1].end_byte()
        } else {
            chord.end_byte()
        };
        let end = self.consume_chord_or_tremolo(children, &mut i, after_duration);
        self.push_event(
            chord.start_byte(),
            end,
            EventKind::Chord(notes),
            duration,
            written,
        );
        i
    }

    /// Consumes a trailing `:` and its chord-modifier or tremolo specification
    /// (`c:maj7`, `c8:32`), so the modifier tokens are not mistaken for notes.
    /// The spec is the run of tokens butting directly against the `:`; a `:`
    /// with nothing adjacent after it is a dangling colon and is flagged.
    /// Returns the byte at which the event now ends.
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

    /// Resolves the pitches inside a `<…>` chord node. In relative mode the
    /// first note is relative to `mode`'s reference and each subsequent note to
    /// the one before it.
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
            notes.push(ChordNote {
                span: Span::new(begin, inner[k - 1].end_byte()),
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

    /// The absolute octave for a note given the mode, written marks, and an
    /// optional octave-check override.
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

    /// Consumes octave marks (`'`/`,`), accidental reminders (`!`/`?`) and an
    /// optional octave check (`='`/`=,`) starting at `children[*i]`. Returns the
    /// net octave shift, whether any marks were written, and the checked octave
    /// (LilyPond's internal value, `c` = -1) if a check was present.
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

    /// Reads an optional duration (number, dots, `*` multipliers) starting at
    /// `children[*i]`. Returns the duration in force and whether it was written
    /// here; updates the inherited duration when it was.
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

    fn push_event(
        &mut self,
        start: usize,
        end: usize,
        kind: EventKind,
        duration: Duration,
        duration_written: bool,
    ) {
        self.events.push(Event {
            span: Span::new(start, end),
            kind,
            duration,
            duration_written,
        });
    }
}

/// LilyPond's relative-octave rule (`Pitch::to_relative_octave`): place the new
/// note name in whichever octave is closest in diatonic steps to `reference`
/// (ties going down), then apply the net written marks.
fn relative_octave(reference: Pitch, note_name: u8, net_marks: i32) -> i32 {
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

fn is_block(kind: &str) -> bool {
    kind == "expression_block" || kind == "parallel_music"
}

/// Collects the byte spans of the outermost `ERROR` nodes in the tree, pruning
/// subtrees that contain no error.
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

/// The text inside a `string` node's quotes, if it has a `string_fragment`.
fn string_fragment<'a>(string_node: Node, src: &'a str) -> Option<&'a str> {
    let mut cursor = string_node.walk();
    string_node
        .named_children(&mut cursor)
        .find(|n| n.kind() == "string_fragment")
        .map(|n| &src[n.start_byte()..n.end_byte()])
}

/// Whether a `\new`/`\context` context type holds something other than note
/// music, so its bare block should not be read as notes.
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
        // The classic: c' then notes chosen as the nearest octave.
        // c' g (down a 4th) c (up a 4th) -> g is below c', c climbs back.
        let analysis = run("\\relative c' { c g c }");
        assert_eq!(pitches(&analysis), vec![(0, 0), (4, -1), (0, 0)]);
    }

    #[test]
    fn relative_with_marks_shifts_octave() {
        // In relative mode each `'` adds an octave on top of the nearest one, so
        // `c''` after c' is two octaves up (octave 2), not absolute c''.
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
        // The rest takes 8 and does not advance the relative reference, so d is
        // still resolved from c.
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

    #[test]
    fn chordmode_symbols_are_left_alone() {
        // We do not resolve pitches in chordmode; the chord-quality symbols
        // (`maj`) must not be flagged as bad notes.
        let analysis = run("\\chordmode { c2:maj7 }");
        assert!(analysis.problems.is_empty());
        assert!(analysis.events.is_empty());
    }

    #[test]
    fn chordmode_propagates_into_nested_blocks() {
        // A bare block nested in chordmode (here a `\repeat` body) stays in
        // chord mode, so its `:min` modifier is not read as a note.
        let analysis = run("\\chordmode { \\repeat unfold 2 { c2:min d } }");
        assert!(analysis.problems.is_empty());
        assert!(analysis.events.is_empty());
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

    #[test]
    fn note_mode_tremolo_is_consumed() {
        // `c8:32` is an eighth note played as a 32nd tremolo; the `:32` must not
        // become a separate event, and the note keeps its `8`.
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
        // The pure suppression predicate, exercised with fabricated spans so it
        // needs no real ERROR node in the tree.
        let errors = [Span::new(10, 20)];
        assert!(under_error(Span::new(12, 15), &errors)); // strictly inside
        assert!(under_error(Span::new(10, 20), &errors)); // exactly the region
        assert!(!under_error(Span::new(5, 9), &errors)); // before
        assert!(!under_error(Span::new(18, 25), &errors)); // straddles the end
        assert!(!under_error(Span::new(0, 5), &[])); // no errors at all
    }

    #[test]
    fn diagnostics_in_a_broken_region_are_suppressed() {
        // This real drum part trips tree-sitter into wrapping the whole input in
        // an ERROR node, so the `\drummode` block never forms and its `sn`/`bd`
        // would otherwise be read as (invalid) notes. Because they fall inside
        // the error region, every such diagnostic is dropped — leaving only the
        // syntax error the parser already reports.
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
