//! Shared note-state helpers for the refactorings that make a note's inherited
//! state explicit: those that move music between contexts (extract-to-variable
//! and inline-variable), where a note that omitted its duration or (in
//! `\relative` mode) its octave would resolve differently once moved, and
//! make-pitches/durations-explicit, which spell out that inherited state in
//! place.
//!
//! These helpers locate the note that carries an event's octave and re-spell its
//! marks, spell the octave adjustment for a relative note, and spell the
//! inherited pitch of a bare duration — the common arithmetic the refactorings
//! use to keep the music sounding the same.

use tower_lsp::lsp_types::{Range, TextEdit};

use crate::document::Document;
use crate::line_struct::Span;
use crate::note_analyser::relative_octave;
use crate::notes::{Event, EventKind, Pitch};

/// The text spelling the inherited pitch of the bare-duration note `events[index]`
/// — what "make pitches explicit" inserts before its duration — or `None` if the
/// note's pitch is already written or no source pitch can be found to copy.
///
/// In `\relative` mode the inherited pitch is the reference in force, so the note
/// name alone (no octave marks) resolves to it. Otherwise — absolute or `\fixed`
/// mode — the previous *written* note's pitch text is copied verbatim: it spells
/// the very same pitch, and being written in the same mode it resolves there to
/// the same pitch again, sidestepping any mode-specific octave arithmetic.
pub(super) fn explicit_pitch(events: &[Event], index: usize, src: &str) -> Option<String> {
    if !matches!(
        events[index].kind,
        EventKind::Note {
            pitch_written: false,
            ..
        }
    ) {
        return None;
    }

    // The nearest earlier note whose pitch the source actually wrote; the bare
    // duration exists only because such a note set the inherited pitch, and in
    // `\relative` mode that note is also the reference now in force.
    let previous = events[..index].iter().rev().find(|event| {
        matches!(
            event.kind,
            EventKind::Note {
                pitch_written: true,
                ..
            }
        )
    })?;
    let value = &src[previous.span.start..previous.value_end];

    // In `\relative` mode the bare note inherits the reference, so its name and
    // accidental alone — no octave marks — resolve back to it. Copying them from
    // the source keeps the writer's own spelling (`ef`, not the language's
    // canonical `e-flat`). Otherwise — absolute or `\fixed` mode — the full pitch
    // text, octave marks included, spells the very same pitch again.
    Some(if events[index].relative.is_some() {
        value[..leading_letters(value)].to_string()
    } else {
        pitch_text(value).to_string()
    })
}

/// The pitch portion at the start of a note's value text: its name and
/// accidental followed by any octave marks (`cis,` of `cis,8`), dropping the
/// duration and the accidental reminders (`!`/`?`) that follow.
fn pitch_text(value: &str) -> &str {
    let head = leading_letters(value);
    let marks = value[head..]
        .bytes()
        .take_while(|b| matches!(b, b'\'' | b','))
        .count();
    &value[..head + marks]
}

/// The resolved pitch carried by an event for relative-octave purposes: a note's
/// pitch, or a chord's first note. `None` for rests, skips and the like.
pub(super) fn pitch_of(event: &Event) -> Option<Pitch> {
    match &event.kind {
        EventKind::Note { pitch, .. } => Some(*pitch),
        EventKind::Chord(notes) | EventKind::ChordRepetition(notes) => {
            notes.first().map(|n| n.pitch)
        }
        _ => None,
    }
}

/// The note whose octave marks carry an event's relative reference — the note
/// itself, or a chord's first note — as its source span and resolved pitch.
/// `None` for events with no such note (rests, skips, chord repetitions). The
/// span is the note value only, excluding post-events, so re-spelling its octave
/// marks never strays into an attached articulation or markup.
pub(super) fn leading_note(event: &Event) -> Option<(Span, Pitch)> {
    match &event.kind {
        // A bare duration carries no note name or octave marks to re-spell, so it
        // is not a leading note even though it resolves to a pitch.
        EventKind::Note {
            pitch,
            pitch_written: true,
            ..
        } => Some((Span::new(event.span.start, event.value_end), *pitch)),
        EventKind::Chord(notes) => notes.first().map(|n| (n.span, n.pitch)),
        _ => None,
    }
}

/// The byte range (within `src`) of the octave marks on the note at `span`, and
/// the marks it should carry so it resolves to `pitch` against reference
/// `outer` — or `None` if the marks are already right or the octave is pinned by
/// a `=` check. The returned span is empty (start == end) when marks are being
/// added where there were none. Covers adding, changing and removing marks.
pub(super) fn octave_respelling(
    src: &str,
    span: Span,
    pitch: Pitch,
    outer: Pitch,
) -> Option<(Span, String)> {
    let text = &src[span.start..span.end];
    // An octave check (`c='`) pins the octave regardless of reference.
    if text.contains('=') {
        return None;
    }
    let head = leading_letters(text);
    let marks_len = text[head..]
        .bytes()
        .take_while(|b| matches!(b, b'\'' | b','))
        .count();
    let wanted = relative_marks(pitch.octave - relative_octave(outer, pitch.note_name, 0));
    if text[head..head + marks_len] == wanted {
        return None;
    }
    Some((
        Span::new(span.start + head, span.start + head + marks_len),
        wanted,
    ))
}

/// An edit re-spelling the octave marks of the note at `span` (resolved pitch
/// `pitch`) so it still resolves to `pitch` against reference `outer`, or `None`
/// if its marks are already right or its octave is pinned by a `=` check. Covers
/// adding, changing and removing marks.
pub(super) fn octave_fix(
    document: &Document,
    span: Span,
    pitch: Pitch,
    outer: Pitch,
) -> Option<TextEdit> {
    let (marks, new_text) = octave_respelling(document.text(), span, pitch, outer)?;
    let line_index = document.line_index();
    Some(TextEdit {
        range: Range::new(
            line_index.position_at(marks.start),
            line_index.position_at(marks.end),
        ),
        new_text,
    })
}

/// The length in bytes of a note's leading name and accidental (`cis` of
/// `cis'8`), i.e. its run of ASCII letters, after which octave marks go.
pub(super) fn leading_letters(text: &str) -> usize {
    text.bytes().take_while(u8::is_ascii_alphabetic).count()
}

/// Octave marks that adjust a relative note by `delta` octaves: `delta` `'`s
/// upward, or `-delta` `,`s downward.
pub(super) fn relative_marks(delta: i32) -> String {
    if delta >= 0 {
        "'".repeat(delta as usize)
    } else {
        ",".repeat((-delta) as usize)
    }
}
