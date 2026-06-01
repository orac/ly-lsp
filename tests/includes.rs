//! Cross-file go-to-definition and find-references through the `\include`
//! graph, exercised against real files on disk.

use std::fs;
use std::path::Path;

use ly_lsp::document_graph::DocumentGraph;
use tower_lsp::lsp_types::{Location, Position, Range, Url};

fn url(path: &Path) -> Url {
    Url::from_file_path(path).expect("absolute path")
}

/// Sorts locations so that result order (which depends on map iteration) does
/// not make assertions flaky.
fn sorted(mut locations: Vec<Location>) -> Vec<Location> {
    locations.sort_by(|a, b| {
        (a.uri.as_str(), a.range.start.line, a.range.start.character).cmp(&(
            b.uri.as_str(),
            b.range.start.line,
            b.range.start.character,
        ))
    });
    locations
}

#[test]
fn goto_definition_follows_include_to_file_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.ily");
    let score = dir.path().join("score.ly");
    fs::write(&notes, "melody = { c d e }\n").unwrap();
    fs::write(&score, "\\include \"notes.ily\"\n\\melody\n").unwrap();

    let ws = DocumentGraph::new();
    // Only the score is open; notes.ily must be read from disk.
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    // Cursor on `\melody` (line 1).
    let locations = ws.goto_definition(&url(&score), Position::new(1, 2));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, url(&notes));
    assert_eq!(
        locations[0].range,
        Range::new(Position::new(0, 0), Position::new(0, 6))
    );
}

#[test]
fn goto_definition_on_include_path_jumps_to_file() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.ily");
    let score = dir.path().join("score.ly");
    fs::write(&notes, "melody = { c }\n").unwrap();
    fs::write(&score, "\\include \"notes.ily\"\n").unwrap();

    let ws = DocumentGraph::new();
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    // Cursor inside the quoted "notes.ily".
    let locations = ws.goto_definition(&url(&score), Position::new(0, 12));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, url(&notes));
    assert_eq!(
        locations[0].range,
        Range::new(Position::new(0, 0), Position::new(0, 0))
    );
}

#[test]
fn resolution_is_transitive_and_cycle_safe() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.ly");
    let b = dir.path().join("b.ily");
    let c = dir.path().join("c.ily");
    // a -> b -> c, and b -> a forms a cycle that must not loop forever.
    fs::write(&a, "\\include \"b.ily\"\n\\tune\n").unwrap();
    fs::write(&b, "\\include \"c.ily\"\n\\include \"a.ly\"\n").unwrap();
    fs::write(&c, "tune = { c d }\n").unwrap();

    let ws = DocumentGraph::new();
    ws.open(url(&a), fs::read_to_string(&a).unwrap());

    // `\tune` in a.ly resolves through b.ily to the definition in c.ily.
    let locations = ws.goto_definition(&url(&a), Position::new(1, 1));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, url(&c));
}

#[test]
fn builtin_command_resolves_to_nothing_across_includes() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.ily");
    let score = dir.path().join("score.ly");
    fs::write(&notes, "melody = { c }\n").unwrap();
    fs::write(&score, "\\include \"notes.ily\"\n\\relative c' { c }\n").unwrap();

    let ws = DocumentGraph::new();
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    // `\relative` is a built-in; no definition anywhere in the graph.
    let locations = ws.goto_definition(&url(&score), Position::new(1, 2));
    assert!(locations.is_empty());
}

#[test]
fn references_span_open_files_that_include_the_definition() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("shared.ily");
    let score1 = dir.path().join("score1.ly");
    let score2 = dir.path().join("score2.ly");
    let other = dir.path().join("other.ly");
    // shared.ily defines `melody`; it is on disk but not opened.
    fs::write(&shared, "melody = { c }\n").unwrap();
    fs::write(&score1, "\\include \"shared.ily\"\n\\melody\n").unwrap();
    fs::write(&score2, "\\include \"shared.ily\"\n\\melody \\melody\n").unwrap();
    // other.ly has its own, unrelated `melody` and does not include shared.ily.
    fs::write(&other, "melody = { d }\n\\melody\n").unwrap();

    let ws = DocumentGraph::new();
    ws.open(url(&score1), fs::read_to_string(&score1).unwrap());
    ws.open(url(&score2), fs::read_to_string(&score2).unwrap());
    ws.open(url(&other), fs::read_to_string(&other).unwrap());

    // Find references from `\melody` in score1.
    let locations = sorted(ws.references(&url(&score1), Position::new(1, 2), false));

    // One from score1, two from score2; none from the unrelated other.ly.
    assert_eq!(
        locations,
        vec![
            Location::new(url(&score1), line_range(1, 0, 7)),
            Location::new(url(&score2), line_range(1, 0, 7)),
            Location::new(url(&score2), line_range(1, 8, 15)),
        ]
    );
}

#[test]
fn references_with_declaration_include_the_definition() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("shared.ily");
    let score = dir.path().join("score.ly");
    fs::write(&shared, "melody = { c }\n").unwrap();
    fs::write(&score, "\\include \"shared.ily\"\n\\melody\n").unwrap();

    let ws = DocumentGraph::new();
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    let locations = sorted(ws.references(&url(&score), Position::new(1, 2), true));
    assert_eq!(
        locations,
        vec![
            // The reference in the open score (score.ly sorts before shared.ily).
            Location::new(url(&score), line_range(1, 0, 7)),
            // The declaration in the (unopened) shared.ily.
            Location::new(url(&shared), line_range(0, 0, 6)),
        ]
    );
}

/// A single-line range from `start` to `end` columns.
fn line_range(line: u32, start: u32, end: u32) -> Range {
    Range::new(Position::new(line, start), Position::new(line, end))
}
