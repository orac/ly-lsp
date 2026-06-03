//! The resolved note-state data model.
//!
//! [`analyse`](crate::note_analyser::analyse) reads a LilyPond document left to
//! right and produces, for each music event, its fully *resolved* pitch and
//! duration — what LilyPond itself would compute, even where the source omits
//! the duration or (in `\relative` mode) the absolute octave. This module holds
//! the types it fills in, stored on the `Document` alongside the symbols and
//! includes; the analysis that builds them lives in [`crate::note_analyser`].

use std::fmt;

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
    /// A single pitched note, e.g. `cis'` (`NoteEvent`).
    Note { pitch: Pitch, octave_written: bool },
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
}

/// A single rhythmic event in the music, with its lexically resolved state.
///
/// The `span` covers the written event — pitch, octave marks, accidental
/// reminders and duration — but not trailing post-events (articulations,
/// dynamics, slurs, ties), which are separate sibling tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub span: Span,
    pub kind: EventKind,
    /// The duration in force for this event, whether written or inherited.
    pub duration: Duration,
    /// Whether the source wrote a duration here, or it was inherited from an
    /// earlier event.
    pub duration_written: bool,
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
#[derive(Debug, Default, PartialEq, Eq)]
pub struct NoteAnalysis {
    /// Every resolved music event, in source order.
    pub events: Events,
    /// Lexical problems found while reading the music, surfaced as diagnostics.
    pub problems: Vec<Problem>,
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

    /// A bare rest event spanning `start..end`, for exercising [`Events`].
    fn rest(start: usize, end: usize) -> Event {
        Event {
            span: Span::new(start, end),
            kind: EventKind::Rest,
            duration: Duration::DEFAULT,
            duration_written: false,
        }
    }

    fn spans(events: &[Event]) -> Vec<(usize, usize)> {
        events.iter().map(|e| (e.span.start, e.span.end)).collect()
    }

    #[test]
    fn events_at_finds_the_containing_event() {
        // Three events with gaps: [0,2), [4,6), [8,10).
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

    #[test]
    fn duration_display_matches_lilypond() {
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
}
