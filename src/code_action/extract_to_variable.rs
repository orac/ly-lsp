//! The "extract to variable" refactoring: turn a selected run of music into a
//! named top-level definition and a `\name` reference in its place.

use tower_lsp::lsp_types::{CodeActionKind, Command, Range, TextEdit};
use tree_sitter::Node;

use crate::document::Document;
use crate::line_struct::Span;
use crate::note_analyser::relative_octave;
use crate::notes::{Duration, Event, EventKind, Pitch};

use super::{CodeAction, Offer, Resolved};

/// The name the new variable is given before the editor prompts to rename it.
const DEFAULT_NAME: &str = "music";

/// The extract-to-variable refactoring. It has no state of its own: `offer` and
/// `resolve` both work from the document and selection, sharing the validity
/// check in [`validate`] so `offer` stays cheap and `resolve` rebuilds lazily.
pub struct ExtractToVariable;

impl CodeAction for ExtractToVariable {
    const ID: &'static str = "extract_to_variable";

    /// Offers the action when `selection` covers an extractable music
    /// expression, declining otherwise — so the menu never lists an edit that
    /// isn't actually available.
    ///
    /// Valid selections are a whole `{ }` block, a single named music event
    /// inside a `{ }` block, or a contiguous run of such events (none partially
    /// covered). The selection boundaries may include surrounding whitespace.
    /// This runs [`validate`] — the same check `resolve` relies on — but stops
    /// there: it never resolves the wrapping mode or builds any edits.
    fn offer(document: &Document, selection: Range) -> Option<Offer> {
        validate(document, selection).map(|_| Offer {
            title: "Extract to variable".to_string(),
            kind: CodeActionKind::REFACTOR_EXTRACT,
        })
    }

    /// Builds the new definition, the `\music` replacement and any following-note
    /// fixes. Re-runs [`validate`] (cheap relative to the work below) so it
    /// stays correct even if the document changed since the menu was shown.
    fn resolve(document: &Document, selection: Range) -> Option<Resolved> {
        let Validated {
            container,
            braces,
            actual_start,
            actual_end,
            selected,
        } = validate(document, selection)?;

        let line_index = document.line_index();
        let text = document.text();

        // Resolving the wrapping mode walks the block ancestry, so it is left to
        // here rather than paid on every `offer`.
        let wrapping = wrapping_for(container, text, braces);

        // Make the variable's first event carry an explicit duration: once it is
        // detached from its surroundings, an omitted duration would inherit from
        // wherever the variable is defined instead of from the original context.
        let mut content = text[actual_start..actual_end].to_string();
        if let Some(first) = selected.first()
            && !first.duration_written
        {
            // The duration goes on the note value, before any post-events, so
            // insert at `value_end` rather than the end of the (possibly
            // articulated) span.
            content.insert_str(first.value_end - actual_start, &first.duration.to_string());
        }

        let mut music_text = format_music_text(&content, wrapping);

        // If the selection sits inside `\relative`, give the variable its own
        // `\relative` so its notes keep their octaves wherever it is used. The
        // reference in force before the selection makes the first note resolve
        // exactly as it did originally.
        if let Some(reference) = selected.first().and_then(|e| e.relative.as_ref()) {
            music_text = format!("\\relative {} {}", reference.text, music_text);
        }

        let replace_range = Range::new(
            line_index.position_at(actual_start),
            line_index.position_at(actual_end),
        );
        let insert_before = line_index.position_at(statement_start_byte(document, container)?);

        let mut edits = vec![
            TextEdit {
                range: Range::new(insert_before, insert_before),
                new_text: format!("{DEFAULT_NAME} = {music_text}\n\n"),
            },
            TextEdit {
                range: replace_range,
                new_text: format!("\\{DEFAULT_NAME}"),
            },
        ];
        edits.extend(following_note_edits(
            document,
            selected,
            actual_start,
            actual_end,
        ));

        Some(Resolved {
            edits,
            // Prompt the editor to rename the freshly inserted `music` variable
            // so the user can give it a meaningful name straight away.
            command: Some(Command {
                title: "Rename".to_string(),
                command: "editor.action.rename".to_string(),
                arguments: None,
            }),
        })
    }
}

/// Whether `start..end` cuts through the note, chord or rest at either edge of
/// `selected`. Only the outermost events need checking: the events between them
/// are bounded by the ends, so if the first and last lie wholly inside the
/// selection, so does everything in between. An empty `selected` — a selection
/// covering no events, such as a bare `\command` — cuts nothing.
fn cuts_boundary(selected: &[Event], start: usize, end: usize) -> bool {
    match (selected.first(), selected.last()) {
        (Some(first), Some(last)) => first.span.start < start || last.span.end > end,
        _ => false,
    }
}

/// The validated shape of an extractable selection: the smallest node enclosing
/// it, whether its music needs `{ }` to stand alone, the whitespace-trimmed byte
/// range, and the events it covers. The full validity check both `offer` and
/// `resolve` rely on — note it stops at *whether* braces are needed and leaves
/// *which* wrapping (the mode) to `resolve`.
struct Validated<'a> {
    container: Node<'a>,
    braces: bool,
    actual_start: usize,
    actual_end: usize,
    selected: &'a [Event],
}

/// Validates `selection` as an extractable music expression, or `None` if it
/// cannot be extracted safely. This is the whole "is the action available?"
/// decision: it rejects an out-of-bounds, empty or syntactically broken
/// selection, one that straddles a single symbol or a `{ }` block (via
/// [`needs_braces`]), and one that cuts through a note at either edge. It builds
/// no edits and does not resolve the wrapping mode.
fn validate(document: &Document, selection: Range) -> Option<Validated<'_>> {
    let line_index = document.line_index();
    let text = document.text();

    let start_byte = line_index.offset_at(selection.start)?;
    let end_byte = line_index.offset_at(selection.end)?;
    if start_byte >= end_byte {
        return None;
    }

    let container = document
        .root_node()
        .descendant_for_byte_range(start_byte, end_byte)?;
    if container.has_error() {
        return None;
    }

    // Reject a selection that slices through a single symbol (half a command
    // name) or a block, or that swallows a brace. `None` means not extractable;
    // the `bool` is whether the music will need wrapping in `{ }`.
    let braces = needs_braces(container, start_byte, end_byte)?;

    // Trim whitespace at the selection edges.
    let raw = &text[start_byte..end_byte];
    let leading = raw.len() - raw.trim_start().len();
    let trailing = raw.len() - raw.trim_end().len();
    let actual_start = start_byte + leading;
    let actual_end = end_byte - trailing;
    if actual_start >= actual_end {
        return None;
    }

    let selected = document.notes().overlapping(actual_start, actual_end);

    // Reject a selection that cuts through a note, chord or rest. This catches a
    // boundary landing inside a multi-token event the tree would otherwise let
    // through, e.g. partway through a `c16:32` tremolo or a `<c e g>` chord.
    if cuts_boundary(selected, actual_start, actual_end) {
        return None;
    }

    // Reject a selection that contains only comments and whitespace. Only
    // applies inside an expression_block; a container that IS the selected node
    // is already a named music node by definition.
    if container.kind() == "expression_block" {
        let mut cur = container.walk();
        let has_music_content = container.named_children(&mut cur).any(|child| {
            child.kind() != "comment"
                && child.start_byte() < actual_end
                && child.end_byte() > actual_start
        });
        if !has_music_content {
            return None;
        }
    }

    Some(Validated {
        container,
        braces,
        actual_start,
        actual_end,
        selected,
    })
}

/// Edits that keep the note *after* the selection meaning the same once the
/// selection is behind a `\music` reference (which leaks neither duration nor
/// relative octave). The note inherits the duration in force *before* the
/// selection, and resolves its octave from the reference there, so an omitted
/// duration or octave that those would change is made explicit.
fn following_note_edits(
    document: &Document,
    selected: &[Event],
    actual_start: usize,
    actual_end: usize,
) -> Vec<TextEdit> {
    let line_index = document.line_index();
    let events = document.notes();

    let mut edits = Vec::new();
    let Some(next) = events.iter().find(|e| e.span.start >= actual_end) else {
        return edits;
    };

    // The next note will now follow the music before the selection, so it is
    // that music's state — not the selection's — that it inherits.
    let before = events.iter().rev().find(|e| e.span.end <= actual_start);
    let duration_before = before.map_or(Duration::DEFAULT, |e| e.duration);

    if !next.duration_written && duration_before != next.duration {
        // As above, the duration belongs on the note value, before its
        // post-events.
        let pos = line_index.position_at(next.value_end);
        edits.push(TextEdit {
            range: Range::new(pos, pos),
            new_text: next.duration.to_string(),
        });
    }

    // The next note's octave only shifts when the selection was relative and
    // the note follows the selection's last note directly in that chain (so its
    // reference was that note, which extraction now replaces with the reference
    // from before the selection). For a chord it is the first note that carries
    // the reference; the rest follow it.
    if let Some(outer) = selected.first().and_then(|e| e.relative.as_ref())
        && next.relative.as_ref().map(|r| r.pitch) == selected.iter().rev().find_map(pitch_of)
        && let Some((span, pitch)) = leading_note(next)
        && let Some(edit) = octave_fix(document, span, pitch, outer.pitch)
    {
        edits.push(edit);
    }

    edits
}

/// An edit re-spelling the octave marks of the note at `span` (resolved pitch
/// `pitch`) so it still resolves to `pitch` against reference `outer`, or `None`
/// if its marks are already right or its octave is pinned by a `=` check. Covers
/// adding, changing and removing marks.
fn octave_fix(document: &Document, span: Span, pitch: Pitch, outer: Pitch) -> Option<TextEdit> {
    let text = &document.text()[span.start..span.end];
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
    let line_index = document.line_index();
    Some(TextEdit {
        range: Range::new(
            line_index.position_at(span.start + head),
            line_index.position_at(span.start + head + marks_len),
        ),
        new_text: wanted,
    })
}

/// Returns the byte offset of the start of the line before which a new top-level
/// definition should be inserted given that `node` is inside or equal to the
/// selected music.
fn statement_start_byte(document: &Document, node: Node<'_>) -> Option<usize> {
    // Walk up to the direct child of lilypond_program.
    let mut top = node;
    loop {
        let parent = top.parent()?;
        if parent.kind() == "lilypond_program" {
            break;
        }
        top = parent;
    }

    // Walk backwards through siblings to find the statement start.
    // Stop when we hit another block (a natural statement boundary) or
    // when we find the assignment_lhs that owns this statement.
    let mut stmt = top;
    let mut scan = top;
    loop {
        match scan.prev_sibling() {
            None => break,
            Some(prev) => match prev.kind() {
                "expression_block" | "parallel_music" => break,
                "assignment_lhs" => {
                    stmt = prev;
                    break;
                }
                _ => {
                    stmt = prev;
                    scan = prev;
                }
            },
        }
    }

    let byte = stmt.start_byte();
    Some(document.text()[..byte].rfind('\n').map_or(0, |i| i + 1))
}

/// Decides whether the music covered by `start_byte..end_byte` must be wrapped
/// in `{ }` to stand alone as the RHS of a definition, given the smallest node
/// `container` that encloses the selection.
///
/// Returns `None` when the selection is not a valid sequential music expression
/// to extract: a partially covered event, a selection that swallows a `{` or
/// `}`, or anything other than a whole `{ }` block or a run of complete events.
fn needs_braces(container: Node<'_>, start_byte: usize, end_byte: usize) -> Option<bool> {
    if container.start_byte() == start_byte && container.end_byte() == end_byte {
        // Case A: selection exactly matches a single node.
        if container.kind() == "expression_block" {
            Some(false)
        } else if container.is_named()
            && container.kind() != "punctuation"
            && container.parent().map(|p| p.kind()) == Some("expression_block")
        {
            // A \command already forms a complete music expression on its own;
            // a bare note or chord needs wrapping.
            Some(container.kind() != "escaped_word")
        } else {
            None
        }
    } else {
        // Case B: selection is a sub-range within an expression_block.
        if container.kind() != "expression_block" || container.child_count() < 2 {
            return None;
        }

        let open = container.child(0)?;
        let close = container.child(container.child_count() - 1)?;

        // The { and } must lie fully outside the selection.
        if start_byte < open.end_byte() || end_byte > close.start_byte() {
            return None;
        }

        // No named child may straddle a selection boundary.
        let mut cur = container.walk();
        let has_partial = container.named_children(&mut cur).any(|child| {
            let overlaps = child.start_byte() < end_byte && child.end_byte() > start_byte;
            let inside = child.start_byte() >= start_byte && child.end_byte() <= end_byte;
            overlaps && !inside
        });
        if has_partial {
            return None;
        }

        // A selection beginning with \command is taken as "command plus
        // arguments" — a single expression that needs no extra braces.
        let mut cur2 = container.walk();
        let first_selected = container
            .named_children(&mut cur2)
            .find(|c| c.start_byte() >= start_byte && c.end_byte() <= end_byte);
        Some(first_selected.map(|n| n.kind()) != Some("escaped_word"))
    }
}

/// How an extracted selection must be wrapped to stand alone as the RHS of a
/// definition.
enum Wrapping {
    /// The selection is already a complete expression; use it verbatim.
    Verbatim,
    /// Wrap in `{ }`.
    Braces,
    /// Wrap in a mode command's block, e.g. `\lyricmode { ... }`. The mode is
    /// re-established so the content parses correctly wherever the variable is
    /// referenced, not just at the original site.
    Mode(&'static str),
}

/// Decides how the validated selection must be wrapped, given whether it
/// `needs_braces` (already determined by [`validate`]). This is the deferred
/// half of the wrapping decision: working out the mode walks the block
/// ancestry, so it is left until the user actually picks the action.
fn wrapping_for(container: Node<'_>, src: &str, needs_braces: bool) -> Wrapping {
    if !needs_braces {
        return Wrapping::Verbatim;
    }
    // Bare events inside a mode block (`\lyricmode { la la }`) are parsed in
    // that mode, so the extracted definition must restate it.
    match enclosing_mode(container, src) {
        Some(mode) => Wrapping::Mode(mode),
        None => Wrapping::Braces,
    }
}

/// If `node` sits inside a mode or context command's block, returns the mode
/// command the extracted music should be wrapped in. `\lyrics`/`\chords` map to
/// their mode forms `\lyricmode`/`\chordmode`.
///
/// The mode set by the innermost enclosing block is in force, so we ascend
/// through enclosing `{ }` blocks until one is the argument of a mode command.
/// Plain music-taking commands like `\repeat` preserve the ambient mode, so a
/// chord selected inside `\chordmode { \repeat 2 { c:m } }` still extracts as
/// `\chordmode`. A nested `\notemode` resets to the default mode and halts the
/// ascent, so music inside it extracts with plain braces even within a mode.
fn enclosing_mode(node: Node<'_>, src: &str) -> Option<&'static str> {
    let mut block = if node.kind() == "expression_block" {
        node
    } else {
        // A bare note or chord selected on its own; its parent is the block.
        enclosing_block(node)?
    };
    loop {
        if let Some(command) = block.prev_sibling()
            && command.kind() == "escaped_word"
            && let Some(effect) = mode_command(command.utf8_text(src.as_bytes()).ok()?)
        {
            return match effect {
                ModeEffect::Restate(mode) => Some(mode),
                ModeEffect::NoteMode => None,
            };
        }
        block = enclosing_block(block)?;
    }
}

/// The nearest ancestor `expression_block` of `node`, if any.
fn enclosing_block(node: Node<'_>) -> Option<Node<'_>> {
    let mut cur = node.parent()?;
    while cur.kind() != "expression_block" {
        cur = cur.parent()?;
    }
    Some(cur)
}

/// The effect a mode or context command has on how nested music is parsed.
enum ModeEffect {
    /// Establishes a non-default mode an extracted definition must restate.
    Restate(&'static str),
    /// Returns to the default note mode, which needs no restating.
    NoteMode,
}

/// Classifies a mode or context command (with its leading backslash), or returns
/// `None` for any command that does not change the parsing mode.
fn mode_command(word: &str) -> Option<ModeEffect> {
    use ModeEffect::{NoteMode, Restate};
    match word {
        "\\lyricmode" | "\\lyrics" => Some(Restate("\\lyricmode")),
        "\\chordmode" | "\\chords" => Some(Restate("\\chordmode")),
        "\\figuremode" | "\\figures" => Some(Restate("\\figuremode")),
        "\\drummode" | "\\drums" => Some(Restate("\\drummode")),
        "\\markup" => Some(Restate("\\markup")),
        "\\notemode" | "\\notes" => Some(NoteMode),
        _ => None,
    }
}

/// Formats `content` as a standalone music expression for the RHS of a new
/// definition, according to `wrapping`. Brace wrapping preserves the
/// indentation of a multi-line selection.
fn format_music_text(content: &str, wrapping: Wrapping) -> String {
    match wrapping {
        Wrapping::Verbatim => content.to_string(),
        Wrapping::Braces => braced(content),
        Wrapping::Mode(mode) => format!("{mode} {}", braced(content)),
    }
}

/// The resolved pitch carried by an event for relative-octave purposes: a note's
/// pitch, or a chord's first note. `None` for rests, skips and the like.
fn pitch_of(event: &Event) -> Option<Pitch> {
    match &event.kind {
        EventKind::Note { pitch, .. } => Some(*pitch),
        EventKind::Chord(notes) | EventKind::ChordRepetition(notes) => {
            notes.first().map(|n| n.pitch)
        }
        _ => None,
    }
}

/// The length in bytes of a note's leading name and accidental (`cis` of
/// `cis'8`), i.e. its run of ASCII letters, after which octave marks go.
fn leading_letters(text: &str) -> usize {
    text.bytes().take_while(u8::is_ascii_alphabetic).count()
}

/// The note whose octave marks carry an event's relative reference — the note
/// itself, or a chord's first note — as its source span and resolved pitch.
/// `None` for events with no such note (rests, skips, chord repetitions). The
/// span is the note value only, excluding post-events, so re-spelling its octave
/// marks never strays into an attached articulation or markup.
fn leading_note(event: &Event) -> Option<(Span, Pitch)> {
    match &event.kind {
        EventKind::Note { pitch, .. } => {
            Some((Span::new(event.span.start, event.value_end), *pitch))
        }
        EventKind::Chord(notes) => notes.first().map(|n| (n.span, n.pitch)),
        _ => None,
    }
}

/// Octave marks that adjust a relative note by `delta` octaves: `delta` `'`s
/// upward, or `-delta` `,`s downward.
fn relative_marks(delta: i32) -> String {
    if delta >= 0 {
        "'".repeat(delta as usize)
    } else {
        ",".repeat((-delta) as usize)
    }
}

/// Wraps `content` in `{ }`, keeping a multi-line selection's indentation.
fn braced(content: &str) -> String {
    if let Some((first, rest)) = content.split_once('\n') {
        let indent: String = rest
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        format!("{{\n{}{}\n{}\n}}", indent, first, rest)
    } else {
        format!("{{ {} }}", content)
    }
}
