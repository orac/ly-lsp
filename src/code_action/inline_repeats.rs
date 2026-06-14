//! The "unfold repeat" refactoring. With the cursor in a `\repeat <kind> <n>`
//! header, expand the body into `n` literal copies and drop the `\repeat`
//! wrapper, processing `\volta` so each filtered copy keeps only the music for
//! its repeat and distributing an `\alternative` block's endings across the
//! copies. The `\alternative` may sit inside the body (modern) or follow it
//! (older), and its endings may be plain `{ }` blocks taken positionally or
//! `\volta`-tagged with the repeats they belong to. The body may be a `{ … }`
//! block or a single braceless note or chord (`\repeat percent 4 c2`). When the
//! body's braces straddle line breaks, the copies are laid out one per line,
//! indented to match the `\repeat`; otherwise they sit on one line.
//!
//! The wrinkle is octaves and durations. LilyPond resolves `\relative` (and
//! duration inheritance) over the *folded* body once, so `\unfoldRepeats`
//! reproduces each note's folded-resolved value in every copy. Literal text
//! duplication would instead re-inherit left-to-right and drift. So we walk the
//! constructed output left to right and, at each event, make its duration or
//! octave explicit exactly where its predecessor *in the output* differs from
//! its predecessor *in the folded reading* — the same re-spelling the inline and
//! extract refactorings use, which no-ops wherever the note is already right.
//!
//! # Known limitations
//!
//! - The body is assumed to be in a single octave-entry mode: the relative
//!   reference is seeded from the first event, so a `\relative`/`\fixed` block
//!   nested inside the repeat body may re-spell wrongly.
//! - A `\volta` nested inside an `\alternative` ending's own music is not
//!   filtered (the ending's `\volta` *tag* is honoured; one in its body is not).

use std::cmp::Reverse;

use tower_lsp::lsp_types::{CodeActionKind, Range, TextEdit};
use tree_sitter::Node;

use crate::command::{Arg, CommandCall, is_block};
use crate::document::Document;
use crate::line_struct::Span;
use crate::notes::{Duration, Event, Pitch};

use super::note_state::{leading_note, octave_fix, octave_respelling};
use super::{CodeAction, Offer, Resolved};

/// The repeat kinds we offer to unfold: every kind but `tremolo`, which is a
/// notational shorthand with no literal expansion. `percent` and `segno` unfold
/// to plain repetition (their special notation is understood to be lost).
const UNFOLDABLE_KINDS: [&str; 4] = ["volta", "unfold", "percent", "segno"];

pub struct InlineRepeats;

impl CodeAction for InlineRepeats {
    const ID: &'static str = "inline_repeats";

    fn offer(document: &Document, selection: Range) -> Option<Offer> {
        repeat_at(document, selection).map(|_| Offer {
            title: "Unfold repeat".to_string(),
            kind: CodeActionKind::REFACTOR_INLINE,
        })
    }

    fn resolve(document: &Document, selection: Range) -> Option<Resolved> {
        let repeat = repeat_at(document, selection)?;
        Some(Resolved {
            edits: unfold(document, &repeat)?,
            command: None,
        })
    }
}

/// A `\repeat <kind> <n> { … }` construct, with its optional trailing
/// `\alternative { … }`, located and validated.
struct Repeat {
    /// First byte of the `\repeat` keyword — where the replacement begins.
    start: usize,
    count: u32,
    /// The music to duplicate: the body's inner extent, truncated before an
    /// `\alternative` that sits inside the body (the modern syntax).
    repeated: Span,
    /// The `\alternative` block (braces included), inside the body or trailing
    /// it, if one is present.
    alternative: Option<Span>,
    /// Last byte of the whole construct — where the replacement ends.
    end: usize,
    /// The music block the construct sits directly in, if any. `None` at the top
    /// level or as an assignment value, where the expansion must be wrapped in
    /// `{ }` to stay a single expression and no following note shares its scope.
    block: Option<Span>,
}

/// Locates and fully validates the repeat the cursor sits in: a `\repeat` whose
/// header (keyword, kind, count) the cursor is on, of an unfoldable kind, with a
/// count of at least one and a body. The whole availability check, so `offer`
/// is just `is_some` and `resolve` re-derives the same thing.
fn repeat_at(document: &Document, selection: Range) -> Option<Repeat> {
    let offset = document.line_index().offset_at(selection.start)?;
    let call = document.commands().iter().find(|call| {
        call.name == "repeat" && {
            let header = call.header();
            header.start <= offset && offset <= header.end
        }
    })?;

    let kind = bare_word(call)?;
    if !UNFOLDABLE_KINDS.contains(&kind) {
        return None;
    }
    let count = count(call).filter(|&n| n >= 1)?;
    let body = call.body()?;
    // The music to repeat is a `{ … }`/`<< … >>` body's inner extent, or the
    // whole single note/chord when the body is braceless (`\repeat percent 4 c2`).
    let body_inner = if is_braced(document.text(), body) {
        inner(body)?
    } else {
        body
    };

    // The `\alternative` may sit inside the body (modern) or follow it (older);
    // an inside one ends the repeated music and the construct closes at the
    // body's brace, a trailing one extends the construct to its own end.
    let alt = find_alternative(document, body);
    let (repeated, end) = match alt {
        Some(alt) if alt.inside => (Span::new(body_inner.start, alt.keyword_start), body.end),
        Some(alt) => (body_inner, alt.block.end),
        None => (body_inner, body.end),
    };

    Some(Repeat {
        start: call.keyword.start,
        count,
        repeated,
        alternative: alt.map(|a| a.block),
        end,
        block: parent_block(document, call.keyword.start),
    })
}

/// A located `\alternative` block: its extent, where its keyword begins, and
/// whether it sits inside the repeat body or trails after it.
#[derive(Clone, Copy)]
struct AltLoc {
    block: Span,
    keyword_start: usize,
    inside: bool,
}

/// The `\alternative` belonging to the repeat whose body is `body`: one inside
/// the body if present, otherwise one immediately following it.
fn find_alternative(document: &Document, body: Span) -> Option<AltLoc> {
    if let Some(call) = document
        .commands()
        .within(body.start, body.end)
        .find(|call| call.name == "alternative")
        && let Some(block) = call.body()
    {
        return Some(AltLoc {
            block,
            keyword_start: call.keyword.start,
            inside: true,
        });
    }

    let src = document.text();
    document
        .commands()
        .iter()
        .filter(|call| call.name == "alternative")
        .find(|call| call.keyword.start >= body.end && is_blank(&src[body.end..call.keyword.start]))
        .and_then(|call| {
            Some(AltLoc {
                block: call.body()?,
                keyword_start: call.keyword.start,
                inside: false,
            })
        })
}

/// The music block the `\repeat` at `keyword_start` sits directly in, if any.
fn parent_block(document: &Document, keyword_start: usize) -> Option<Span> {
    let parent = document
        .root_node()
        .descendant_for_byte_range(keyword_start, keyword_start + 1)?
        .parent()?;
    is_block(parent.kind()).then(|| Span::new(parent.start_byte(), parent.end_byte()))
}

/// Builds the edits that replace the construct with its unfolded music, plus any
/// fix to the note following it.
fn unfold(document: &Document, repeat: &Repeat) -> Option<Vec<TextEdit>> {
    let src = document.text();
    let repeated = repeat.repeated;
    let endings = repeat
        .alternative
        .map(|alt| parse_endings(document, alt))
        .unwrap_or_default();

    // Seed the running predecessor from the state in force just before the body:
    // the first body event carries the relative reference and inherited duration.
    let body_events: Vec<&Event> = events_in(document, repeated);
    let relative = body_events.first().is_some_and(|e| e.relative.is_some());
    let mut pred = Pred {
        pitch: body_events
            .first()
            .filter(|_| relative)
            .and_then(|e| e.relative.as_ref().map(|r| r.pitch)),
        duration: body_events
            .first()
            .map_or(Duration::DEFAULT, |e| e.duration),
    };

    let voltas: Vec<&CommandCall> = document
        .commands()
        .within(repeated.start, repeated.end)
        .filter(|call| call.name == "volta")
        .collect();

    let mut segments: Vec<String> = Vec::new();
    for r in 1..=repeat.count {
        segments.push(render_body(
            src,
            repeated,
            &body_events,
            &voltas,
            r,
            relative,
            &mut pred,
        ));
        if let Some(ending) = ending_for(r, repeat.count, &endings) {
            let span = endings[ending].span;
            segments.push(render(
                src,
                span,
                &events_in(document, span),
                Vec::new(),
                relative,
                &mut pred,
            ));
        }
    }

    // A body whose braces straddle line breaks unfolds with each copy on its own
    // line, indented to match the `\repeat` it replaces; a single-line body keeps
    // the copies on one line separated by a space.
    let separator = if src[repeated.start..repeated.end].contains('\n') {
        format!("\n{}", line_indent(src, repeat.start))
    } else {
        " ".to_string()
    };
    let joined = segments
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(&separator);
    let new_text = match repeat.block {
        Some(_) => joined,
        None => format!("{{ {joined} }}"),
    };

    let mut edits = vec![TextEdit {
        range: document
            .line_index()
            .range_of(Span::new(repeat.start, repeat.end)),
        new_text,
    }];
    edits.extend(following_note_edits(document, repeat, &pred, relative));
    Some(edits)
}

/// The running resolved state of the output as we lay copies down: the last
/// pitched note's pitch (the `\relative` reference) and the duration in force.
struct Pred {
    pitch: Option<Pitch>,
    duration: Duration,
}

/// Renders one copy of the body for repeat `r`: the body inner text with dropped
/// `\volta` blocks removed, kept ones unwrapped, and per-event duration/octave
/// fixes applied. Advances `pred` through the kept events.
fn render_body(
    src: &str,
    body: Span,
    events: &[&Event],
    voltas: &[&CommandCall],
    r: u32,
    relative: bool,
    pred: &mut Pred,
) -> String {
    let mut structural: Vec<(usize, usize, String)> = Vec::new();
    for volta in voltas {
        // A volta inside a dropped one is already removed with it; skip it.
        if voltas
            .iter()
            .any(|outer| contains(outer, volta) && !plays_on(outer, r))
        {
            continue;
        }
        if plays_on(volta, r) {
            // Keep the music, unwrap the `\volta N { ` … ` }` around it; keep
            // the trimmed content so no doubled space is left behind.
            let Some(body) = volta.body() else { continue };
            let Some(content) = trimmed_inner(src, body) else {
                continue;
            };
            structural.push((volta.keyword.start, content.start, String::new()));
            structural.push((content.end, body.end, String::new()));
        } else {
            // Drop the whole `\volta N { … }`, swallowing the space after it so
            // the surrounding music keeps a single separator.
            let body_end = volta.body().map_or(volta.span.end, |b| b.end);
            let end = body_end + horizontal_ws(&src[body_end..body.end]);
            structural.push((volta.keyword.start, end, String::new()));
        }
    }

    let kept: Vec<&Event> = events
        .iter()
        .copied()
        .filter(|e| !dropped(e, voltas, r))
        .collect();
    render(src, body, &kept, structural, relative, pred)
}

/// Renders a run of `events` taken from the source span `inner`, applying the
/// given `structural` edits (volta unwrapping) and the per-event duration and
/// octave fixes that keep each note's folded-resolved value. All edit spans lie
/// within `inner`. Advances `pred`.
fn render(
    src: &str,
    inner: Span,
    events: &[&Event],
    mut edits: Vec<(usize, usize, String)>,
    relative: bool,
    pred: &mut Pred,
) -> String {
    for event in events {
        if !event.duration_written && event.duration != pred.duration {
            edits.push((event.value_end, event.value_end, event.duration.to_string()));
        }
        pred.duration = event.duration;

        if relative && let Some((span, pitch)) = leading_note(event) {
            if let Some(outer) = pred.pitch
                && let Some((marks, text)) = octave_respelling(src, span, pitch, outer)
            {
                edits.push((marks.start, marks.end, text));
            }
            pred.pitch = Some(pitch);
        }
    }

    let mut text = src[inner.start..inner.end].to_string();
    edits.sort_by_key(|&(start, ..)| Reverse(start));
    for (start, end, replacement) in edits {
        text.replace_range(start - inner.start..end - inner.start, &replacement);
    }
    text.trim().to_string()
}

/// A fix to the note following the construct: now preceded by the last copy's
/// last note rather than the body's lexically last note, it may need its
/// duration or octave made explicit to keep its folded-resolved value.
fn following_note_edits(
    document: &Document,
    repeat: &Repeat,
    pred: &Pred,
    relative: bool,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    // Only a note sharing the construct's block inherits from it; one past the
    // block boundary (or at the top level) resolves in a different scope.
    let Some(block) = repeat.block else {
        return edits;
    };
    let Some(after) = document
        .notes()
        .iter()
        .find(|e| e.span.start >= repeat.end && e.span.end <= block.end)
    else {
        return edits;
    };

    if !after.duration_written && after.duration != pred.duration {
        let pos = document.line_index().position_at(after.value_end);
        edits.push(TextEdit {
            range: Range::new(pos, pos),
            new_text: after.duration.to_string(),
        });
    }

    if relative
        && let Some(outer) = pred.pitch
        && let Some((span, pitch)) = leading_note(after)
        && let Some(edit) = octave_fix(document, span, pitch, outer)
    {
        edits.push(edit);
    }

    edits
}

/// One alternative ending: the music to play and, if it was written as a
/// `\volta`-tagged block, the repeats it plays on. A plain `{ … }` ending has no
/// list and is placed positionally instead.
struct Ending {
    span: Span,
    voltas: Option<Vec<u32>>,
}

/// Which ending (0-based) repeat `r` of `n` uses, or `None` if none applies.
/// `\volta`-tagged endings are matched by their number list; plain endings are
/// distributed positionally, the first ending filling the earliest repeats when
/// there are fewer endings than repeats.
fn ending_for(r: u32, n: u32, endings: &[Ending]) -> Option<usize> {
    if endings.is_empty() {
        return None;
    }
    if endings.iter().any(|e| e.voltas.is_some()) {
        return endings
            .iter()
            .position(|e| e.voltas.as_ref().is_some_and(|v| v.contains(&r)));
    }
    let m = endings.len() as u32;
    let one_based = if m >= n {
        r.min(m)
    } else {
        (r + m).saturating_sub(n).max(1)
    };
    Some((one_based as usize - 1).min(endings.len() - 1))
}

/// Whether event `e` is dropped on repeat `r`: it lies inside a `\volta` block
/// that does not play on `r`.
fn dropped(e: &Event, voltas: &[&CommandCall], r: u32) -> bool {
    voltas.iter().any(|volta| {
        !plays_on(volta, r)
            && volta
                .body()
                .is_some_and(|b| b.start <= e.span.start && e.span.end <= b.end)
    })
}

/// Whether the `\volta` call plays on repeat `r` (its number list contains `r`).
fn plays_on(volta: &CommandCall, r: u32) -> bool {
    volta
        .args
        .iter()
        .find_map(|arg| match arg {
            Arg::NumberList { values, .. } => Some(values.contains(&r)),
            _ => None,
        })
        .unwrap_or(false)
}

/// Whether `outer`'s body strictly contains `inner`'s span.
fn contains(outer: &CommandCall, inner: &CommandCall) -> bool {
    outer.span != inner.span
        && outer
            .body()
            .is_some_and(|b| b.start <= inner.keyword.start && inner.span.end <= b.end)
}

/// The events lying wholly within `span`, in source order.
fn events_in(document: &Document, span: Span) -> Vec<&Event> {
    document
        .notes()
        .overlapping(span.start, span.end)
        .iter()
        .filter(|e| e.span.start >= span.start && e.span.end <= span.end)
        .collect()
}

/// The endings inside an `\alternative` block. If it holds `\volta`-tagged
/// blocks, those are the endings, each with its number list; otherwise the plain
/// `{ … }` blocks are the endings, taken positionally.
fn parse_endings(document: &Document, alt_block: Span) -> Vec<Ending> {
    let voltas: Vec<&CommandCall> = document
        .commands()
        .within(alt_block.start, alt_block.end)
        .filter(|call| call.name == "volta")
        .collect();
    // Only the `\volta`s directly in the block are endings; one nested in another
    // ending's music is not.
    let top_level: Vec<&CommandCall> = voltas
        .iter()
        .copied()
        .filter(|v| !voltas.iter().any(|outer| contains(outer, v)))
        .collect();
    if !top_level.is_empty() {
        return top_level
            .iter()
            .filter_map(|volta| {
                Some(Ending {
                    span: inner(volta.body()?)?,
                    voltas: Some(volta_numbers(volta)),
                })
            })
            .collect();
    }

    let Some(node) = block_node(document, alt_block) else {
        return Vec::new();
    };
    let mut cursor = node.walk();
    let positional: Vec<Ending> = node
        .children(&mut cursor)
        .filter(|child| is_block(child.kind()))
        .filter_map(|child| {
            Some(Ending {
                span: inner(Span::new(child.start_byte(), child.end_byte()))?,
                voltas: None,
            })
        })
        .collect();
    // An `\alternative` with no `{ }` endings and no `\volta` tags is a single
    // ending — its whole content — rather than nothing.
    if positional.is_empty() {
        return inner(alt_block)
            .map(|span| vec![Ending { span, voltas: None }])
            .unwrap_or_default();
    }
    positional
}

/// The repeat numbers listed by a `\volta` call (`\volta 2,3` → `[2, 3]`).
fn volta_numbers(call: &CommandCall) -> Vec<u32> {
    call.args
        .iter()
        .find_map(|arg| match arg {
            Arg::NumberList { values, .. } => Some(values.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The block node covering exactly `span`.
fn block_node(document: &Document, span: Span) -> Option<Node<'_>> {
    let mut node = document
        .root_node()
        .descendant_for_byte_range(span.start, span.end)?;
    while !is_block(node.kind()) {
        node = node.parent()?;
    }
    Some(node)
}

/// The inner extent of a block with surrounding whitespace trimmed, or `None` if
/// the block is too small to hold delimiters.
fn trimmed_inner(src: &str, block: Span) -> Option<Span> {
    let inner = inner(block)?;
    let text = &src[inner.start..inner.end];
    let lead = text.len() - text.trim_start().len();
    let trail = text.len() - text.trim_end().len();
    Some(Span::new(
        inner.start + lead,
        (inner.end - trail).max(inner.start + lead),
    ))
}

/// The byte length of the leading run of spaces and tabs in `text` (stopping at a
/// newline, so a dropped `\volta` on its own line doesn't pull the next line up).
fn horizontal_ws(text: &str) -> usize {
    text.bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count()
}

/// The trimmed inner extent of a `{ … }`/`<< … >>` block span (delimiters
/// excluded), or `None` if it is too small to hold one.
fn inner(block: Span) -> Option<Span> {
    // `{`/`}` are one byte; `<<`/`>>` two. Detect from the span text later if we
    // need parallel music; voltas and repeat bodies are brace blocks.
    let (open, close) = (block.start + 1, block.end.checked_sub(1)?);
    if open > close {
        return None;
    }
    Some(Span::new(open, close))
}

/// The first `BareWord` argument's text (the repeat kind).
fn bare_word(call: &CommandCall) -> Option<&str> {
    call.args.iter().find_map(|arg| match arg {
        Arg::BareWord { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

/// The first `Count` argument's value (the repeat count).
fn count(call: &CommandCall) -> Option<u32> {
    call.args.iter().find_map(|arg| match arg {
        Arg::Count { value, .. } => Some(*value),
        _ => None,
    })
}

/// Whether the span opens with a block delimiter (`{` or `<<`), distinguishing a
/// braced body from a braceless single note or `<…>` chord. A lone `<` opens a
/// chord, not a parallel-music block, so only a doubled `<<` counts.
fn is_braced(src: &str, span: Span) -> bool {
    let bytes = src.as_bytes();
    bytes.get(span.start) == Some(&b'{')
        || (bytes.get(span.start) == Some(&b'<') && bytes.get(span.start + 1) == Some(&b'<'))
}

/// The leading whitespace of the line containing `offset` — the indentation an
/// unfolded copy takes so it lines up under the `\repeat` it replaces.
fn line_indent(src: &str, offset: usize) -> &str {
    let line_start = src[..offset].rfind('\n').map_or(0, |n| n + 1);
    let line = &src[line_start..offset];
    &line[..line.len() - line.trim_start().len()]
}

fn is_blank(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}
