//! The resolved note-state data model.
//!
//! [`analyse`](crate::note_analyser::analyse) reads a LilyPond document left to
//! right and produces, for each music event, its fully *resolved* pitch and
//! duration — what LilyPond itself would compute, even where the source omits
//! the duration or (in `\relative` mode) the absolute octave. This module holds
//! the types it fills in, stored on the `Document` alongside the symbols and
//! includes; the analysis that builds them lives in [`crate::note_analyser`].

use std::fmt;

use crate::command::Commands;
use crate::line_struct::Span;

/// A pitch resolved to its absolute value, mirroring LilyPond's internal model
/// (`lily/pitch.cc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pitch {
    /// Diatonic note name: `0 = c`, `1 = d`, … `6 = b`.
    pub note_name: u8,
    /// Octave, in LilyPond's convention: middle C (`c'`) is `0`, the bare `c`
    /// below it is `-1`. Each `'` adds one and each `,` subtracts one.
    pub octave: i32,
    /// Alteration in quarter-tone steps: natural `0`, sharp `+2`, flat `-2`,
    /// double-sharp `+4`, half-sharp (`cis` vs `cih`) `+1`. LilyPond stores this
    /// as a `Rational` in whole-tone units (a semitone is `1/2`); this is that
    /// value times four, which is integral for every note name LilyPond defines.
    pub alteration: i8,
}

/// A duration resolved to its absolute value, mirroring LilyPond's `Duration`
/// (`lily/duration.cc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Duration {
    /// Base note value as the negated log2 of the whole-note fraction:
    /// `0` = whole, `1` = half, `2` = quarter, `3` = eighth, … and negative for
    /// `\breve` (`-1`) and `\longa` (`-2`).
    pub log: i32,
    /// Number of augmentation dots.
    pub dots: u8,
    /// The `*N/M` multiplier as a fraction, `(1, 1)` when none is written. Like
    /// the log and dots, the factor is inherited by a following note that omits
    /// its duration (`c4*2/3 d` makes `d` a `4*2/3` too).
    pub factor: (u32, u32),
}

impl Duration {
    /// LilyPond's default before any duration has been seen: a quarter note.
    pub const DEFAULT: Duration = Duration {
        log: 2,
        dots: 0,
        factor: (1, 1),
    };
}

impl fmt::Display for Duration {
    /// Renders the duration in LilyPond's own notation, e.g. `8.*3/2`,
    /// `4`, `\breve`, `1*4`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.log {
            -1 => f.write_str("\\breve")?,
            -2 => f.write_str("\\longa")?,
            -3 => f.write_str("\\maxima")?,
            log => write!(f, "{}", 1u64 << log.max(0))?,
        }
        for _ in 0..self.dots {
            f.write_str(".")?;
        }
        match self.factor {
            (1, 1) => {}
            (n, 1) => write!(f, "*{n}")?,
            (n, d) => write!(f, "*{n}/{d}")?,
        }
        Ok(())
    }
}

/// One pitch within a chord (`<c e g>`). Each carries its own span because a
/// refactoring may want to annotate inner notes individually, and in `\relative`
/// mode each inner note is resolved relative to the one before it.
///
/// The `span` extends over any post-events attached to the inner note — a
/// fingering or articulation such as the `-1` of `<c-1 e>` — so the note and
/// what hangs off it are treated as one unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChordNote {
    pub span: Span,
    pub pitch: Pitch,
    /// Whether the source wrote octave marks on this note (only meaningful in
    /// `\relative` mode, where their absence means "nearest octave").
    pub octave_written: bool,
}

/// What kind of music event a token run resolved to, named after LilyPond's own
/// music events (`NoteEvent`, `RestEvent`, `SkipEvent`, …). Rests, skips and
/// multi-measure rests carry a duration but no pitch, and do not advance the
/// `\relative` reference pitch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// A single pitched note, e.g. `cis'` (`NoteEvent`). A bare duration that
    /// repeats the previous note's pitch — the `4` in `c4 4` — is also a `Note`,
    /// carrying that inherited pitch with `pitch_written` false.
    Note {
        pitch: Pitch,
        octave_written: bool,
        /// Whether the source wrote a note name here, or the pitch was inherited
        /// from the previous note (a bare duration). When inherited, the octave
        /// marks are necessarily absent too, so `octave_written` is false.
        pitch_written: bool,
    },
    /// A chord, e.g. `<c e g>` (`EventChord`). The duration lives on the
    /// enclosing [`Event`].
    Chord(Vec<ChordNote>),
    /// A rest, `r` (`RestEvent`).
    Rest,
    /// A multi-measure rest, `R` (`MultiMeasureRestEvent`).
    MultiMeasureRest,
    /// A skip / spacer rest, `s` (`SkipEvent`).
    Skip,
    /// Chord repetition, `q`: repeats the pitches of the previous chord. The
    /// repeated pitches are filled in here for convenience, though `q` itself
    /// does not change the `\relative` reference pitch.
    ChordRepetition(Vec<ChordNote>),
    /// A chord-mode entry: a root with its chord-quality modifier and
    /// bass/inversion (`c:maj7/e`). Chord mode gives symbols chord meanings
    /// rather than pitches, so the root is not resolved to a [`Pitch`] — only
    /// the entry's extent and its (possibly inherited) [`Duration`] are
    /// recorded, enough for a refactoring to keep the entry whole and write its
    /// duration explicitly when detaching it. The duration sits between the root
    /// and the `:`/`/`, so [`value_end`](Event::value_end) marks the point
    /// before them, not the end of the whole entry.
    ChordModeEvent,
}

/// The `\relative` reference in force just before an event: the reference
/// pitch (for octave arithmetic) and a spelling of it in the active note-name
/// language (for a generated `\relative` wrapper, e.g. `c'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelativeRef {
    pub pitch: Pitch,
    pub text: String,
}

/// A single rhythmic event in the music, with its lexically resolved state.
///
/// The `span` covers the whole written event: its note value — pitch, octave
/// marks, accidental reminders and duration — together with any post-events
/// attached to it (articulations, fingerings, dynamics, slurs, ties and text or
/// markup scripts), which the grammar leaves as separate sibling tokens. A
/// refactoring can therefore treat the run as one unit and refuse to split a
/// note from its articulations. The post-events' content is not interpreted;
/// only their extent is recorded. [`value_end`](Self::value_end) marks where
/// the note value stops and the post-events begin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub span: Span,
    pub kind: EventKind,
    /// The byte offset at which the note value ends and any post-events begin,
    /// equal to `span.end` when there are none. An omitted duration must be
    /// written here — on the value, before the post-events — not at `span.end`.
    /// For a [`ChordModeEvent`](EventKind::ChordModeEvent) it marks the point
    /// before the `:quality`/`/bass`, so an inserted duration reads `c4:m`.
    pub value_end: usize,
    /// The duration in force for this event, whether written or inherited.
    pub duration: Duration,
    /// Whether the source wrote a duration here, or it was inherited from an
    /// earlier event.
    pub duration_written: bool,
    /// In `\relative` mode, the reference in force just before this event;
    /// `None` in absolute and `\fixed` modes, where octaves don't depend on a
    /// reference. Lets a refactoring re-establish the octave when the event's
    /// surroundings change.
    pub relative: Option<RelativeRef>,
}

/// The resolved music events of a document, kept in source order. Events never
/// overlap, so the sequence is also sorted by span, and a position or span can
/// be located by binary search rather than a linear scan — what the refactoring
/// queries need.
///
/// Derefs to `[Event]`, so the events can also be iterated and indexed directly.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Events {
    events: Vec<Event>,
}

impl Events {
    /// Wraps events already in source order with disjoint spans (as the analyser
    /// produces them).
    pub fn new(events: Vec<Event>) -> Self {
        debug_assert!(
            events.windows(2).all(|w| w[0].span.end <= w[1].span.start),
            "events must be in source order with non-overlapping spans"
        );
        Self { events }
    }

    /// The event whose span contains `offset`, if any.
    pub fn at(&self, offset: usize) -> Option<&Event> {
        let after = self.events.partition_point(|e| e.span.start <= offset);
        self.events[..after]
            .last()
            .filter(|e| e.span.contains(offset))
    }

    /// The events overlapping the half-open byte range `start..end`, as a
    /// contiguous slice (empty when the range touches no event).
    pub fn overlapping(&self, start: usize, end: usize) -> &[Event] {
        let lo = self.events.partition_point(|e| e.span.end <= start);
        let hi = self.events.partition_point(|e| e.span.start < end);
        &self.events[lo..hi]
    }
}

impl std::ops::Deref for Events {
    type Target = [Event];

    fn deref(&self) -> &Self::Target {
        &self.events
    }
}

/// The outcome of the lexical note-state pass over a document.
#[derive(Debug, Default)]
pub struct NoteAnalysis {
    /// Every resolved music event, in source order.
    pub events: Events,
    /// Lexical problems found while reading the music, surfaced as diagnostics.
    pub problems: Vec<Problem>,
    /// The structured command invocations (`\repeat`, `\volta`, …) recognised in
    /// the same pass, for the refactorings and completion to share.
    pub commands: Commands,
}

/// A diagnosable problem found by the lexical note pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// A bare symbol at an event position that is no note name in the language
    /// active there (and is not `r`/`R`/`s`/`q`).
    NotANote(Span),
    /// A `:` not followed by a chord modifier or tremolo specification.
    DanglingColon(Span),
}

impl Problem {
    pub fn span(self) -> Span {
        match self {
            Problem::NotANote(span) | Problem::DanglingColon(span) => span,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A bare rest event spanning `start..end`, for exercising [`Events`].
    fn rest(start: usize, end: usize) -> Event {
        Event {
            span: Span::new(start, end),
            kind: EventKind::Rest,
            value_end: end,
            duration: Duration::DEFAULT,
            duration_written: false,
            relative: None,
        }
    }

    fn spans(events: &[Event]) -> Vec<(usize, usize)> {
        events.iter().map(|e| (e.span.start, e.span.end)).collect()
    }

    #[test]
    fn events_at_finds_the_containing_event() {
        // Three events with gaps: [0,2), [4,6), [8,10). Kept as a literal
        // regression anchor alongside the property test below: it documents
        // the half-open-span edge cases (the boundary at 2, the gap at 3) in
        // one readable place rather than relying on shrinking to rediscover
        // them.
        let events = Events::new(vec![rest(0, 2), rest(4, 6), rest(8, 10)]);
        assert_eq!(events.at(0).map(|e| e.span.start), Some(0));
        assert_eq!(events.at(1).map(|e| e.span.start), Some(0));
        assert_eq!(events.at(2), None); // span is half-open, so 2 is past [0,2)
        assert_eq!(events.at(3), None); // in the gap
        assert_eq!(events.at(5).map(|e| e.span.start), Some(4));
        assert_eq!(events.at(100), None);
    }

    #[test]
    fn events_overlapping_returns_the_touched_run() {
        // Kept alongside the property test below for the same reason as
        // `events_at_finds_the_containing_event`: it pins the specific
        // boundary-touching cases in the contract's prose.
        let events = Events::new(vec![rest(0, 2), rest(4, 6), rest(8, 10)]);
        // A range falling entirely in a gap touches nothing.
        assert!(events.overlapping(2, 4).is_empty());
        // A range straddling two events returns both.
        assert_eq!(spans(events.overlapping(5, 9)), vec![(4, 6), (8, 10)]);
        // Touching only the start byte of an event counts as overlap.
        assert_eq!(spans(events.overlapping(1, 5)), vec![(0, 2), (4, 6)]);
        // The whole document.
        assert_eq!(events.overlapping(0, 10).len(), 3);
    }

    /// A naive linear scan for the event containing `offset`, obviously
    /// correct by inspection, to check `Events::at`'s binary search against.
    fn at_naive(events: &[Event], offset: usize) -> Option<&Event> {
        events.iter().find(|e| e.span.contains(offset))
    }

    /// A naive linear scan for the events overlapping `start..end`, to check
    /// `Events::overlapping`'s binary search against.
    fn overlapping_naive(events: &[Event], start: usize, end: usize) -> Vec<(usize, usize)> {
        events
            .iter()
            .filter(|e| e.span.start < end && start < e.span.end)
            .map(|e| (e.span.start, e.span.end))
            .collect()
    }

    /// Builds disjoint, ascending spans from a list of `(gap, length)` pairs,
    /// each accumulated onto the previous end — the shape `Events::new`
    /// requires — and wraps them as rest events.
    fn events_from_gaps(gaps: Vec<(usize, usize)>) -> Events {
        let mut cursor = 0;
        let events = gaps
            .into_iter()
            .map(|(gap, length)| {
                let start = cursor + gap;
                let end = start + length.max(1);
                cursor = end;
                rest(start, end)
            })
            .collect();
        Events::new(events)
    }

    proptest! {
        /// `Events::at` binary-searches for the containing event; a linear
        /// scan is obviously correct, so the two must always agree, for any
        /// arrangement of disjoint spans and any query offset.
        #[test]
        fn events_at_agrees_with_a_linear_scan(
            gaps in prop::collection::vec((0usize..5, 1usize..5), 0..20),
            offset in 0usize..200,
        ) {
            let events = events_from_gaps(gaps);
            let want = at_naive(&events, offset).map(|e| (e.span.start, e.span.end));
            let got = events.at(offset).map(|e| (e.span.start, e.span.end));
            prop_assert_eq!(got, want);
        }

        /// `Events::overlapping` binary-searches too; check it against the
        /// same kind of linear-scan oracle, and check the documented part of
        /// its contract that the result is always a contiguous run of the
        /// underlying event list (not just the right *set* of events).
        #[test]
        fn events_overlapping_agrees_with_a_linear_scan_and_stays_contiguous(
            gaps in prop::collection::vec((0usize..5, 1usize..5), 0..20),
            a in 0usize..200,
            b in 0usize..200,
        ) {
            let events = events_from_gaps(gaps);
            let (start, end) = (a.min(b), a.max(b));
            let want = overlapping_naive(&events, start, end);
            let got = events.overlapping(start, end);
            prop_assert_eq!(spans(got), want);

            // Contiguity: the returned slice, if non-empty, must be exactly
            // one run of the source events, identifiable by matching spans
            // of its first and last element back into the full list.
            if let (Some(first), Some(last)) = (got.first(), got.last()) {
                let lo = events.iter().position(|e| e.span == first.span).unwrap();
                let hi = events.iter().position(|e| e.span == last.span).unwrap();
                prop_assert_eq!(hi - lo + 1, got.len());
            }
        }
    }

    #[test]
    fn duration_display_matches_lilypond() {
        // Kept as a literal pin of LilyPond's own notation for each duration
        // shape, alongside the property test below that covers the general
        // structure.
        assert_eq!(
            Duration {
                log: 2,
                dots: 0,
                factor: (1, 1)
            }
            .to_string(),
            "4"
        );
        assert_eq!(
            Duration {
                log: 3,
                dots: 1,
                factor: (3, 2)
            }
            .to_string(),
            "8.*3/2"
        );
        assert_eq!(
            Duration {
                log: 0,
                dots: 0,
                factor: (4, 1)
            }
            .to_string(),
            "1*4"
        );
        assert_eq!(
            Duration {
                log: -1,
                dots: 0,
                factor: (1, 1)
            }
            .to_string(),
            "\\breve"
        );
    }

    proptest! {
        /// Whatever the log, dots and factor, the rendered duration must be
        /// non-empty, carry exactly as many trailing dots as `dots` says, and
        /// carry a `*` multiplier if and only if the factor isn't the
        /// implicit `(1, 1)` — the structural shape the four pinned examples
        /// above are each instances of.
        #[test]
        fn duration_display_has_the_right_dots_and_star(
            log in -3i32..=6,
            dots in 0u8..=3,
            n in 1u32..=9,
            d in 1u32..=9,
        ) {
            let duration = Duration { log, dots, factor: (n, d) };
            let rendered = duration.to_string();
            prop_assert!(!rendered.is_empty());

            let dot_count = rendered.chars().filter(|&c| c == '.').count();
            prop_assert_eq!(dot_count, dots as usize);

            let has_star = rendered.contains('*');
            prop_assert_eq!(has_star, (n, d) != (1, 1));
        }
    }
}
