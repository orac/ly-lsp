//! The "make explicit" refactorings: spell out the note state the source leaves
//! to inheritance, so a passage reads the same in isolation as in place.
//!
//! Three actions share the machinery, differing only in what they write:
//!
//! - [`MakeDurationsExplicit`] writes the inherited duration on every event in
//!   the selection that omits one (`c4 d e` → `c4 d4 e4`).
//! - [`MakePitchesExplicit`] writes the inherited pitch on every bare duration
//!   (`c4 4 4` → `c4 c4 c4`), with octave marks outside a `\relative` block.
//! - [`MakeBothExplicit`] does both.
//!
//! Each draws on the resolved note state the analyser already computes: an event
//! carries its resolved duration and whether the source wrote it, and a bare
//! duration is a [`Note`](crate::notes::EventKind::Note) whose pitch was
//! inherited. Spelling that inherited pitch back is the one fiddly part, and
//! lives in the shared [`note_state`](super::note_state) module.

use tower_lsp::lsp_types::{CodeActionKind, Range, TextEdit};

use crate::document::Document;
use crate::notes::{Event, EventKind};

use super::note_state::explicit_pitch;
use super::{CodeAction, Offer, Resolved};

/// Write the inherited duration on every selected event that omits one.
pub struct MakeDurationsExplicit;

/// Write the inherited pitch on every bare duration in the selection.
pub struct MakePitchesExplicit;

/// Write both the inherited pitch and duration throughout the selection.
pub struct MakeBothExplicit;

impl CodeAction for MakeDurationsExplicit {
    const ID: &'static str = "make_durations_explicit";

    fn offer(document: &Document, selection: Range) -> Option<Offer> {
        offer_if(document, selection, false, true, "Make durations explicit")
    }

    fn resolve(document: &Document, selection: Range) -> Option<Resolved> {
        resolve_edits(document, selection, false, true)
    }
}

impl CodeAction for MakePitchesExplicit {
    const ID: &'static str = "make_pitches_explicit";

    fn offer(document: &Document, selection: Range) -> Option<Offer> {
        offer_if(document, selection, true, false, "Make pitches explicit")
    }

    fn resolve(document: &Document, selection: Range) -> Option<Resolved> {
        resolve_edits(document, selection, true, false)
    }
}

impl CodeAction for MakeBothExplicit {
    const ID: &'static str = "make_pitches_and_durations_explicit";

    fn offer(document: &Document, selection: Range) -> Option<Offer> {
        offer_if(
            document,
            selection,
            true,
            true,
            "Make pitches and durations explicit",
        )
    }

    fn resolve(document: &Document, selection: Range) -> Option<Resolved> {
        resolve_edits(document, selection, true, true)
    }
}

/// Offers the action titled `title` when at least one selected event would gain
/// a pitch (`pitches`) or a duration (`durations`).
fn offer_if(
    document: &Document,
    selection: Range,
    pitches: bool,
    durations: bool,
    title: &str,
) -> Option<Offer> {
    let events = document.notes();
    selected_indices(document, selection)
        .iter()
        .any(|&i| (durations && omits_duration(&events[i])) || (pitches && omits_pitch(&events[i])))
        .then(|| Offer {
            title: title.to_string(),
            kind: CodeActionKind::REFACTOR_REWRITE,
        })
}

/// Builds the insertions that make the selected events explicit, `None` if the
/// selection has nothing to add (so a stale offer resolves to no edit).
fn resolve_edits(
    document: &Document,
    selection: Range,
    pitches: bool,
    durations: bool,
) -> Option<Resolved> {
    let events = document.notes();
    let line_index = document.line_index();
    let src = document.text();
    let mut edits = Vec::new();

    // In source order, so the per-event pitch (before the duration) and duration
    // (after the value) insertions come out sorted and non-overlapping.
    for i in selected_indices(document, selection) {
        let event = &events[i];
        if pitches && let Some(text) = explicit_pitch(events, i, src) {
            let at = line_index.position_at(event.span.start);
            edits.push(TextEdit {
                range: Range::new(at, at),
                new_text: text,
            });
        }
        if durations && omits_duration(event) {
            let at = line_index.position_at(event.value_end);
            edits.push(TextEdit {
                range: Range::new(at, at),
                new_text: event.duration.to_string(),
            });
        }
    }

    (!edits.is_empty()).then_some(Resolved {
        edits,
        command: None,
    })
}

/// Whether `event` inherited its duration rather than writing one.
fn omits_duration(event: &Event) -> bool {
    !event.duration_written
}

/// Whether `event` is a note whose pitch was inherited (a bare duration).
fn omits_pitch(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Note {
            pitch_written: false,
            ..
        }
    )
}

/// The indices, into the document's events, of those the selection touches. A
/// collapsed selection (a bare cursor) picks the event it sits in; a range picks
/// every event it overlaps.
fn selected_indices(document: &Document, selection: Range) -> Vec<usize> {
    let line_index = document.line_index();
    let (Some(start), Some(end)) = (
        line_index.offset_at(selection.start),
        line_index.offset_at(selection.end),
    ) else {
        return Vec::new();
    };
    document
        .notes()
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            if start == end {
                event.span.contains(start)
            } else {
                event.span.start < end && event.span.end > start
            }
        })
        .map(|(i, _)| i)
        .collect()
}
