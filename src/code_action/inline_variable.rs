//! The "inline variable" refactorings: the inverse of extract-to-variable.
//! Replace a `\foo` reference with the music `foo` is defined as, making the
//! first note's duration and octave explicit so the spliced music keeps its
//! meaning in its new surroundings.
//!
//! Two actions share the machinery:
//!
//! - [`InlineAll`] is offered on a definition's left-hand side (`foo = …`) or on
//!   any `\foo` reference. It inlines every reference and deletes the definition.
//! - [`InlineHere`] is offered only on a `\foo` reference. It inlines just that
//!   one reference, leaving the definition in place.
//!
//! Like extract, the analyser gives every note a fully *resolved* pitch and
//! duration, which is what lets the splice be made explicit. The key observation
//! for octaves: because each note in a `\relative` run is resolved relative to
//! its (unchanged) predecessor, splicing a `\relative` definition into another
//! `\relative` chain only changes the reference for the run's *first* note — so
//! only that note's octave marks need re-spelling.

use std::cmp::Reverse;

use tower_lsp::lsp_types::{CodeActionKind, Range, TextEdit};
use tree_sitter::Node;

use crate::document::Document;
use crate::line_struct::Span;
use crate::notes::{Duration, Event};

use super::note_state::{leading_note, octave_fix, octave_respelling, pitch_of};
use super::{CodeAction, Offer, Resolved};

/// Inline every reference to the definition the cursor names and delete the
/// definition. Offered both on the definition and on any of its references.
pub struct InlineAll;

/// Inline just the `\foo` reference under the cursor, leaving the definition.
pub struct InlineHere;

impl CodeAction for InlineAll {
    const ID: &'static str = "inline_all";

    fn offer(document: &Document, selection: Range) -> Option<Offer> {
        // Available wherever a definition can be named: on its left-hand side, or
        // on a reference that resolves to one.
        named_definition(document, selection).map(|_| Offer {
            title: "Inline all".to_string(),
            kind: CodeActionKind::REFACTOR_INLINE,
        })
    }

    fn resolve(document: &Document, selection: Range) -> Option<Resolved> {
        let def = named_definition(document, selection)?;
        let mut edits = Vec::new();
        for reference in reference_spans(document, &def.name) {
            // Refuse the whole action if any reference can't be inlined, rather
            // than delete the definition and leave a reference dangling.
            edits.extend(inline_at(document, &def, reference)?);
        }
        edits.push(TextEdit {
            range: document.line_index().range_of(def.delete),
            new_text: String::new(),
        });
        Some(Resolved {
            edits,
            command: None,
        })
    }
}

impl CodeAction for InlineHere {
    const ID: &'static str = "inline_here";

    fn offer(document: &Document, selection: Range) -> Option<Offer> {
        // Only offer where the reference actually resolves to a local
        // definition; a built-in like `\relative` has nothing to inline.
        let (name, _) = reference_target(document, selection)?;
        find_definition(document, &name).map(|_| Offer {
            title: "Inline here".to_string(),
            kind: CodeActionKind::REFACTOR_INLINE,
        })
    }

    fn resolve(document: &Document, selection: Range) -> Option<Resolved> {
        let (name, reference) = reference_target(document, selection)?;
        let def = find_definition(document, &name)?;
        Some(Resolved {
            edits: inline_at(document, &def, reference)?,
            command: None,
        })
    }
}

/// A top-level `name = rhs` definition, with everything inlining needs.
struct Definition {
    name: String,
    /// The half-open byte region from the left-hand side to the end of the `=`
    /// (and the space after it), i.e. the `foo = ` the cursor may sit in.
    lhs_region: Span,
    /// The trimmed extent of the right-hand side text.
    rhs: Span,
    shape: RhsShape,
    /// The bytes "Inline all" removes: the whole definition and its trailing
    /// blank line.
    delete: Span,
}

/// The shape of a definition's right-hand side, as far as splicing it cares.
enum RhsShape {
    /// A single `{ … }` block; `inner` is the trimmed extent inside the braces,
    /// spliced bare when the reference sits in a music block.
    Block { inner: Span },
    /// A `\relative <ref> { … }` block; `inner` is the trimmed extent inside the
    /// inner braces, spliced bare (re-spelling octaves) when the reference is in
    /// `\relative` mode too. Otherwise the whole `\relative …` is kept.
    RelativeBlock { inner: Span },
    /// Anything else (a bare `\command`, `\repeat … { }`, `<< … >>`): spliced
    /// verbatim, braces and all.
    Other,
}

/// The definition the cursor names, whether through its left-hand side or a
/// reference to it. `None` if the cursor is on neither, or on a reference with
/// no matching top-level definition (e.g. a built-in command).
fn named_definition(document: &Document, selection: Range) -> Option<Definition> {
    let byte = document.line_index().offset_at(selection.start)?;
    if let Some(def) = definitions(document)
        .into_iter()
        .find(|def| def.lhs_region.contains(byte))
    {
        // Nothing to inline into if the definition is unused; leave deleting a
        // dead variable to the user rather than dressing it up as "inline".
        return (!reference_spans(document, &def.name).is_empty()).then_some(def);
    }
    let (name, _) = reference_target(document, selection)?;
    find_definition(document, &name)
}

/// The name and span of the `\foo` reference under the cursor, if any.
fn reference_target(document: &Document, selection: Range) -> Option<(String, Span)> {
    let byte = document.line_index().offset_at(selection.start)?;
    document
        .references()
        .iter()
        .find(|symbol| symbol.span.contains(byte))
        .map(|symbol| (symbol.name.clone(), symbol.span))
}

/// The definition named `name`, if a single top-level one exists.
fn find_definition(document: &Document, name: &str) -> Option<Definition> {
    definitions(document).into_iter().find(|d| d.name == name)
}

/// The byte spans of every `\name` reference in the document.
fn reference_spans(document: &Document, name: &str) -> Vec<Span> {
    document
        .references()
        .iter()
        .filter(|symbol| symbol.name == name)
        .map(|symbol| symbol.span)
        .collect()
}

/// Every top-level `name = rhs` definition in source order.
fn definitions(document: &Document) -> Vec<Definition> {
    let src = document.text();
    let root = document.root_node();
    let mut cursor = root.walk();
    let children: Vec<Node> = root.children(&mut cursor).collect();

    let mut defs = Vec::new();
    let mut i = 0;
    while i < children.len() {
        if children[i].kind() != "assignment_lhs" {
            i += 1;
            continue;
        }
        let lhs = children[i];
        let name = lhs
            .utf8_text(src.as_bytes())
            .ok()
            .map(str::to_string)
            .unwrap_or_default();

        // Skip the `=` to find the RHS nodes. The flat grammar makes the RHS a
        // run of siblings rather than one node, so bound it at the first
        // complete music expression: consume up to and including the first
        // block (the value of `{ … }`, `\chordmode { … }`, `\relative c' { … }`,
        // …), stopping early at the next top-level assignment. A bare-command
        // value with no block (`foo = \bar`) runs to that next statement.
        let mut j = i + 1;
        if children.get(j).map(Node::kind) == Some("punctuation") {
            j += 1;
        }
        let rhs_first = j;
        while j < children.len() && children[j].kind() != "assignment_lhs" {
            let kind = children[j].kind();
            j += 1;
            if kind == "expression_block" || kind == "parallel_music" {
                break;
            }
        }
        let rhs_nodes = &children[rhs_first..j];

        if let Some(def) = build_definition(name, lhs, rhs_nodes, src) {
            defs.push(def);
        }
        i = j;
    }
    defs
}

/// Assembles a [`Definition`] from its left-hand side and right-hand side nodes,
/// or `None` if the right-hand side is empty.
fn build_definition(name: String, lhs: Node, rhs_nodes: &[Node], src: &str) -> Option<Definition> {
    let first = rhs_nodes.first()?;
    let last = rhs_nodes.last()?;

    // The RHS text, trimmed (the nodes already exclude leading/trailing space,
    // but be defensive).
    let raw = &src[first.start_byte()..last.end_byte()];
    let lead = raw.len() - raw.trim_start().len();
    let trail = raw.len() - raw.trim_end().len();
    let rhs = Span::new(first.start_byte() + lead, last.end_byte() - trail);

    let shape = rhs_shape(rhs_nodes, src);
    let lhs_region = Span::new(lhs.start_byte(), rhs.start);
    let delete = delete_span(lhs.start_byte(), rhs.end, src);

    Some(Definition {
        name,
        lhs_region,
        rhs,
        shape,
        delete,
    })
}

/// Classifies the right-hand side from its nodes.
fn rhs_shape(rhs_nodes: &[Node], src: &str) -> RhsShape {
    if let [only] = rhs_nodes
        && only.kind() == "expression_block"
    {
        return RhsShape::Block {
            inner: block_inner(*only, src),
        };
    }
    if let (Some(first), Some(last)) = (rhs_nodes.first(), rhs_nodes.last())
        && first.kind() == "escaped_word"
        && &src[first.start_byte()..first.end_byte()] == "\\relative"
        && last.kind() == "expression_block"
    {
        return RhsShape::RelativeBlock {
            inner: block_inner(*last, src),
        };
    }
    RhsShape::Other
}

/// The trimmed byte extent inside a block's delimiters.
fn block_inner(block: Node, src: &str) -> Span {
    let count = block.child_count();
    let (start, end) = match (
        block.child(0),
        count.checked_sub(1).and_then(|n| block.child(n)),
    ) {
        (Some(open), Some(close)) => (open.end_byte(), close.start_byte()),
        _ => (block.start_byte(), block.end_byte()),
    };
    let raw = &src[start..end];
    let lead = raw.len() - raw.trim_start().len();
    let trail = raw.len() - raw.trim_end().len();
    Span::new(start + lead, end - trail)
}

/// The bytes to remove when deleting the definition starting at `lhs_start` and
/// ending at `rhs_end`: from the start of its line through its trailing newline,
/// and one following blank line if present.
fn delete_span(lhs_start: usize, rhs_end: usize, src: &str) -> Span {
    let start = src[..lhs_start].rfind('\n').map_or(0, |i| i + 1);
    let mut end = rhs_end;
    let bytes = src.as_bytes();
    // Through the end of the RHS line.
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    if end < bytes.len() {
        end += 1; // the newline itself
    }
    // Swallow one following blank line (whitespace then newline, or EOF).
    let mut scan = end;
    while scan < bytes.len() && (bytes[scan] == b' ' || bytes[scan] == b'\t') {
        scan += 1;
    }
    if scan >= bytes.len() || bytes[scan] == b'\n' {
        end = (scan + 1).min(bytes.len());
    }
    Span::new(start, end)
}

/// The edits that splice `def` into the reference at `reference`: the
/// replacement of the `\foo` token, plus any duration/octave fixes to the note
/// after it. `None` if the reference can't be located in the tree.
fn inline_at(document: &Document, def: &Definition, reference: Span) -> Option<Vec<TextEdit>> {
    let text = document.text();
    let line_index = document.line_index();
    let events = document.notes();

    let reference_node = document
        .root_node()
        .descendant_for_byte_range(reference.start, reference.end)?;
    let block = reference_node
        .parent()
        .filter(|p| matches!(p.kind(), "expression_block" | "parallel_music"));

    // The events flanking the reference within its own block, used both to read
    // the mode in force and to chain octaves/durations across the splice.
    let (before, after) = match block {
        Some(block) => (
            events
                .iter()
                .rev()
                .find(|e| e.span.end <= reference.start && e.span.start >= block.start_byte()),
            events
                .iter()
                .find(|e| e.span.start >= reference.end && e.span.end <= block.end_byte()),
        ),
        None => (None, None),
    };
    let in_relative =
        before.is_some_and(|e| e.relative.is_some()) || after.is_some_and(|e| e.relative.is_some());

    // Choose the text to splice and whether the spliced notes join the
    // reference's `\relative` chain (so their octaves must be re-spelled).
    let (content_span, strip_relative) = match &def.shape {
        RhsShape::Block { inner } if block.is_some() => (*inner, false),
        RhsShape::RelativeBlock { inner } if block.is_some() && in_relative => (*inner, true),
        _ => (def.rhs, false),
    };
    let mut content = text[content_span.start..content_span.end].to_string();

    let def_events = events.overlapping(content_span.start, content_span.end);

    // Edits within `content`, as (start, end, replacement) byte offsets relative
    // to `content_span.start`.
    let mut inner: Vec<(usize, usize, String)> = Vec::new();

    // Make the first note's duration explicit if, detached from the definition,
    // it would now inherit a different duration from the music before the
    // reference.
    if let Some(first) = def_events.first()
        && !first.duration_written
    {
        let inherited = before.map_or(Duration::DEFAULT, |e| e.duration);
        if first.duration != inherited {
            let at = first.value_end - content_span.start;
            inner.push((at, at, first.duration.to_string()));
        }
    }

    // Re-spell the first pitched note's octave so it resolves the same against
    // the reference in force at the splice point rather than the definition's.
    if strip_relative
        && let Some(outer) = before
            .and_then(pitch_of)
            .or_else(|| after.and_then(|e| e.relative.as_ref().map(|r| r.pitch)))
        && let Some((span, pitch)) = def_events.iter().find_map(leading_note)
        && let Some((marks, new_text)) = octave_respelling(text, span, pitch, outer)
    {
        inner.push((
            marks.start - content_span.start,
            marks.end - content_span.start,
            new_text,
        ));
    }

    // Apply the inner edits latest-first so earlier offsets stay valid.
    inner.sort_by_key(|&(start, ..)| Reverse(start));
    for (start, end, replacement) in inner {
        content.replace_range(start..end, &replacement);
    }

    let mut edits = vec![TextEdit {
        range: line_index.range_of(reference),
        new_text: content,
    }];
    edits.extend(following_note_edits(
        document,
        def_events,
        after,
        strip_relative,
    ));
    Some(edits)
}

/// Edits keeping the note *after* the reference meaning the same once the
/// definition's music sits before it. The note now inherits the definition's
/// last duration and resolves its octave against the definition's last note, so
/// an omitted duration or octave that those change is made explicit.
fn following_note_edits(
    document: &Document,
    def_events: &[Event],
    after: Option<&Event>,
    strip_relative: bool,
) -> Vec<TextEdit> {
    let line_index = document.line_index();
    let mut edits = Vec::new();
    let Some(after) = after else {
        return edits;
    };

    // The definition's last note now precedes `after`, so it is the definition's
    // trailing state that `after` inherits.
    if let Some(last) = def_events.last()
        && !after.duration_written
        && last.duration != after.duration
    {
        let pos = line_index.position_at(after.value_end);
        edits.push(TextEdit {
            range: Range::new(pos, pos),
            new_text: after.duration.to_string(),
        });
    }

    // Only a spliced `\relative` run advances the reference; otherwise `after`
    // still resolves against whatever preceded the reference.
    if strip_relative
        && let Some(new_reference) = def_events.iter().rev().find_map(pitch_of)
        && let Some((span, pitch)) = leading_note(after)
        && let Some(edit) = octave_fix(document, span, pitch, new_reference)
    {
        edits.push(edit);
    }

    edits
}
