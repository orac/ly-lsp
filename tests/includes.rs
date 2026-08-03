//! Cross-file go-to-definition and find-references through the `\include`
//! graph, exercised against real files on disk.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use ly_lsp::document_graph::DocumentGraph;
use tower_lsp::lsp_types::{DiagnosticSeverity, Location, Position, Range, Url};

fn url(path: &Path) -> Url {
    Url::from_file_path(path).expect("absolute path")
}

/// Forces a file's modification time, so cache-invalidation behaviour can be
/// tested without depending on filesystem timestamp granularity.
fn set_mtime(path: &Path, time: SystemTime) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(time).unwrap();
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

#[test]
fn undefined_reference_is_flagged_but_builtins_and_includes_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let words = dir.path().join("lilypond-words");
    let shared = dir.path().join("shared.ily");
    let score = dir.path().join("score.ly");
    // Built-in commands carry a doubled backslash; context/grob names don't.
    fs::write(&words, "\\\\relative\n\\\\new\nStaff\nScore\n").unwrap();
    fs::write(&shared, "melody = { c }\n").unwrap();
    // `\relative` is a builtin, `\melody` is defined in the include, `\wibble`
    // is neither.
    fs::write(
        &score,
        "\\include \"shared.ily\"\n\\relative c' { \\melody }\n\\wibble\n",
    )
    .unwrap();

    let ws = DocumentGraph::new();
    assert!(ws.load_vocabulary(&words));
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    let diagnostics = ws.diagnostics(&url(&score));
    assert_eq!(diagnostics.len(), 1, "only \\wibble should be flagged");
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
    assert!(diagnostic.message.contains("\\wibble"));
    assert_eq!(diagnostic.range, line_range(2, 0, 7));
}

#[test]
fn include_resolves_through_search_path() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let lib = dir.path().join("lib");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&lib).unwrap();

    let common = lib.join("common.ily");
    let score = project.join("score.ly");
    // common.ily lives only in the lib directory, reachable via `-I lib`.
    fs::write(&common, "melody = { c }\n").unwrap();
    fs::write(&score, "\\include \"common.ily\"\n\\melody\n").unwrap();

    let ws = DocumentGraph::new();
    ws.set_search_paths(vec![lib.clone()]);
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    // Go-to-definition on `\melody` finds the file in the search path.
    let locations = ws.goto_definition(&url(&score), Position::new(1, 2));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, url(&common));

    // And go-to-definition on the include path itself jumps there too.
    let to_file = ws.goto_definition(&url(&score), Position::new(0, 12));
    assert_eq!(to_file.len(), 1);
    assert_eq!(to_file[0].uri, url(&common));
}

#[test]
fn including_directory_takes_precedence_over_search_path() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let lib = dir.path().join("lib");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&lib).unwrap();

    // The same filename exists in both places; the local one should win.
    let local = project.join("common.ily");
    fs::write(&local, "melody = { c }\n").unwrap();
    fs::write(lib.join("common.ily"), "melody = { d }\n").unwrap();
    let score = project.join("score.ly");
    fs::write(&score, "\\include \"common.ily\"\n\\melody\n").unwrap();

    let ws = DocumentGraph::new();
    ws.set_search_paths(vec![lib]);
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    let locations = ws.goto_definition(&url(&score), Position::new(1, 2));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, url(&local));
}

#[test]
fn opening_a_cached_include_uses_the_live_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let common = dir.path().join("common.ily");
    let score = dir.path().join("score.ly");
    fs::write(&common, "melody = { c }\n").unwrap();
    fs::write(&score, "\\include \"common.ily\"\n\\melody\n\\tune\n").unwrap();

    let ws = DocumentGraph::new();
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    // Resolving through the include caches the on-disk common.ily.
    assert_eq!(
        ws.goto_definition(&url(&score), Position::new(1, 2)).len(),
        1
    );
    assert!(
        ws.goto_definition(&url(&score), Position::new(2, 2))
            .is_empty()
    );

    // The editor opens common.ily, and its buffer differs from disk (melody
    // renamed to tune). The live buffer must win over the cached parse.
    ws.open(url(&common), "tune = { c }\n".to_string());

    assert!(
        ws.goto_definition(&url(&score), Position::new(1, 2))
            .is_empty(),
        "\\melody should stop resolving once the open buffer drops it"
    );
    let tune = ws.goto_definition(&url(&score), Position::new(2, 2));
    assert_eq!(tune.len(), 1, "\\tune from the open buffer should resolve");
    assert_eq!(tune[0].uri, url(&common));
}

#[test]
fn include_cache_serves_until_mtime_changes() {
    let dir = tempfile::tempdir().unwrap();
    let common = dir.path().join("common.ily");
    let score = dir.path().join("score.ly");
    fs::write(&common, "melody = { c }\n").unwrap();
    fs::write(&score, "\\include \"common.ily\"\n\\melody\n\\tune\n").unwrap();
    let t0 = fs::metadata(&common).unwrap().modified().unwrap();

    let ws = DocumentGraph::new();
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    // Populate the cache: `\melody` resolves into common.ily.
    assert_eq!(
        ws.goto_definition(&url(&score), Position::new(1, 2)).len(),
        1
    );

    // Rewrite the file (melody -> tune) but force the mtime back: the stale
    // cached parse should still be served.
    fs::write(&common, "tune = { c }\n").unwrap();
    set_mtime(&common, t0);
    assert_eq!(
        ws.goto_definition(&url(&score), Position::new(1, 2)).len(),
        1,
        "stale cache should still resolve \\melody"
    );
    assert!(
        ws.goto_definition(&url(&score), Position::new(2, 2))
            .is_empty(),
        "the new \\tune definition is invisible while the cache is stale"
    );

    // Advance the mtime: the cache is invalidated and the new content wins.
    set_mtime(&common, t0 + Duration::from_secs(2));
    assert!(
        ws.goto_definition(&url(&score), Position::new(1, 2))
            .is_empty(),
        "\\melody should be gone after the re-read"
    );
    assert_eq!(
        ws.goto_definition(&url(&score), Position::new(2, 2)).len(),
        1,
        "\\tune should resolve after the re-read"
    );
}

#[test]
fn music_function_defined_in_an_included_file_resolves_across_the_include_graph() {
    // `myFunc = #(define-music-function …)` is an ordinary assignment to the
    // symbol query, so it resolves through `\include` exactly like any other
    // variable — go-to-definition finds it, and no undefined-reference
    // diagnostic is raised for `\myFunc`. See `doc/command-parsing.md`'s
    // "Go-to-definition for user-defined music functions" table.
    let dir = tempfile::tempdir().unwrap();
    let words = dir.path().join("lilypond-words");
    let functions = dir.path().join("functions.ily");
    let score = dir.path().join("score.ly");
    fs::write(&words, "\\\\relative\n").unwrap();
    fs::write(
        &functions,
        "myFunc = #(define-music-function (m) (ly:music?) m)\n",
    )
    .unwrap();
    fs::write(&score, "\\include \"functions.ily\"\n\\myFunc { c4 }\n").unwrap();

    let ws = DocumentGraph::new();
    assert!(ws.load_vocabulary(&words));
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    // Cursor on `\myFunc` (line 1).
    let locations = ws.goto_definition(&url(&score), Position::new(1, 2));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, url(&functions));
    assert_eq!(
        locations[0].range,
        Range::new(Position::new(0, 0), Position::new(0, 6))
    );

    // And it must not be flagged as an undefined reference.
    let diagnostics = ws.diagnostics(&url(&score));
    assert!(
        diagnostics.is_empty(),
        "expected no diagnostics, got {diagnostics:?}"
    );
}

#[test]
fn a_music_function_from_an_include_gives_its_calls_a_signature() {
    // The signature of `\myFunc` lives in another file entirely, so this tests finding symbols and applying their signatures across the whole Scope.
    let dir = tempfile::tempdir().unwrap();
    let functions = dir.path().join("functions.ily");
    let score = dir.path().join("score.ly");
    fs::write(
        &functions,
        "myFunc = #(define-music-function (from music) (ly:pitch? ly:music?)\n  \"Do a @var{thing}.\"\n  music)\n",
    )
    .unwrap();
    fs::write(&score, "\\include \"functions.ily\"\n\\myFunc c' { c4 }\n").unwrap();

    let ws = DocumentGraph::new();
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    // Cursor just after `\myFunc `, where the first argument goes.
    let help = ws
        .signature_help(&url(&score), Position::new(1, 8))
        .expect("signature help for a user-defined function");
    assert_eq!(help.signatures[0].label, "\\myFunc from music");
    assert_eq!(help.active_parameter, Some(0));

    // And hovering the keyword shows the docstring, converted from Texinfo.
    let hover = ws
        .hover(&url(&score), Position::new(1, 3))
        .expect("hover on the function name");
    let rendered = format!("{:?}", hover.contents);
    assert!(rendered.contains("Do a *thing*."), "got {rendered}");
}

#[test]
fn editing_an_include_re_analyses_the_calls_in_the_files_that_include_it() {
    let dir = tempfile::tempdir().unwrap();
    let functions = dir.path().join("functions.ily");
    let score = dir.path().join("score.ly");
    fs::write(
        &functions,
        "myFunc = #(define-music-function (music) (ly:music?) music)\n",
    )
    .unwrap();
    fs::write(&score, "\\include \"functions.ily\"\n\\myFunc { c4 }\n").unwrap();

    let ws = DocumentGraph::new();
    ws.open(url(&score), fs::read_to_string(&score).unwrap());
    assert_eq!(
        ws.signature_help(&url(&score), Position::new(1, 8))
            .expect("signature help")
            .signatures[0]
            .label,
        "\\myFunc music"
    );

    // The user opens the include and gives the function a leading pitch. The
    // score is untouched, but its calls must be read against the new signature.
    ws.open(
        url(&functions),
        "myFunc = #(define-music-function (from music) (ly:pitch? ly:music?) music)\n".to_string(),
    );
    assert_eq!(
        ws.signature_help(&url(&score), Position::new(1, 8))
            .expect("signature help")
            .signatures[0]
            .label,
        "\\myFunc from music"
    );
}

#[test]
fn a_function_defined_only_inside_scheme_resolves_like_any_other() {
    // `#(define-public myFunc (define-music-function …))` has no
    // `assignment_lhs`, so the document's symbol query never sees it; the
    // Scheme reader reports the binding instead, name and span alike. Across
    // the include graph that has to behave exactly like `myFunc = …`: no
    // undefined-reference diagnostic, and go-to-definition landing on the name
    // inside the `#( … )`.
    let dir = tempfile::tempdir().unwrap();
    let words = dir.path().join("lilypond-words");
    let functions = dir.path().join("functions.ily");
    let score = dir.path().join("score.ly");
    fs::write(&words, "\\\\relative\n").unwrap();
    fs::write(
        &functions,
        "#(define-public myFunc (define-music-function (m) (ly:music?) m))\n",
    )
    .unwrap();
    fs::write(&score, "\\include \"functions.ily\"\n\\myFunc { c4 }\n").unwrap();

    let ws = DocumentGraph::new();
    assert!(ws.load_vocabulary(&words));
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    let diagnostics = ws.diagnostics(&url(&score));
    assert!(
        diagnostics.is_empty(),
        "expected no diagnostics, got {diagnostics:?}"
    );
    // `#(define-public myFunc …)`: the name starts at column 16 of line 0.
    let locations = ws.goto_definition(&url(&score), Position::new(1, 2));
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, url(&functions));
    assert_eq!(locations[0].range, line_range(0, 16, 22));
}

#[test]
fn undefined_reference_diagnostics_are_off_without_vocabulary() {
    let dir = tempfile::tempdir().unwrap();
    let score = dir.path().join("score.ly");
    // `\wibble` is undefined, but with no vocabulary loaded we cannot tell it
    // from a built-in command, so we stay silent.
    fs::write(&score, "\\wibble\n").unwrap();

    let ws = DocumentGraph::new();
    ws.open(url(&score), fs::read_to_string(&score).unwrap());

    assert!(ws.diagnostics(&url(&score)).is_empty());
}
