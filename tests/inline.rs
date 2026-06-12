use ly_lsp::code_action::CodeAction;
use ly_lsp::code_action::inline_variable::{InlineAll, InlineHere};
use ly_lsp::document::Document;
use ly_lsp::line_struct::LineIndex;
use tower_lsp::lsp_types::{Position, Range, TextEdit};

// Applies the action's edits to `src`. The edits are non-overlapping, so
// applying them from the latest offset to the earliest keeps the untouched
// offsets valid.
fn apply_edits(src: &str, edits: &[TextEdit]) -> String {
    let idx = LineIndex::new(src);
    let mut spliced: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| {
            (
                idx.offset_at(edit.range.start).unwrap(),
                idx.offset_at(edit.range.end).unwrap(),
                edit.new_text.as_str(),
            )
        })
        .collect();
    spliced.sort_by_key(|&(start, ..)| std::cmp::Reverse(start));

    let mut out = src.to_string();
    for (start, end, text) in spliced {
        out.replace_range(start..end, text);
    }
    out
}

/// Which inline action a case exercises.
#[derive(Clone, Copy, PartialEq)]
enum Which {
    All,
    Here,
}

struct Case {
    source: String,
    cursor: Position,
    which: Which,
    expected: Option<String>,
    line: usize,
}

fn parse_file(content: &str) -> Vec<Case> {
    let mut current_line = 1usize;
    let mut cases = Vec::new();
    for block in content.split("\n===\n") {
        let leading_newlines = block.bytes().take_while(|&b| b == b'\n').count();
        let trimmed = block.trim_matches('\n');
        if !trimmed.is_empty() {
            cases.push(parse_case(trimmed, current_line + leading_newlines));
        }
        current_line += block.bytes().filter(|&b| b == b'\n').count() + 2;
    }
    cases
}

// A case is the annotated source, then `---`, then the expected output (or the
// word NONE). The annotation is a line of leading spaces, a `^`, then `all` or
// `here`, placed below the source line the cursor sits on.
fn parse_case(text: &str, line: usize) -> Case {
    let (annotated, expected_raw) = text
        .split_once("\n---\n")
        .unwrap_or_else(|| panic!("case missing '---' separator:\n{text}"));

    let mut source_lines: Vec<&str> = Vec::new();
    let mut cursor: Option<(usize, u32)> = None;
    let mut which = Which::Here;

    for line in annotated.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('^') {
            let col = (line.len() - trimmed.len()) as u32;
            let row = source_lines.len().saturating_sub(1);
            which = match rest.trim() {
                "all" => Which::All,
                "here" | "" => Which::Here,
                other => panic!("unknown inline action `{other}` in:\n{text}"),
            };
            cursor = Some((row, col));
        } else {
            source_lines.push(line);
        }
    }

    let (row, col) = cursor.unwrap_or_else(|| panic!("case missing `^` annotation:\n{text}"));
    let source = source_lines.join("\n") + "\n";
    let expected_str = expected_raw.trim();
    Case {
        source,
        cursor: Position::new(row as u32, col),
        which,
        expected: (expected_str != "NONE").then(|| expected_str.to_string()),
        line,
    }
}

#[test]
fn inline_cases() {
    let dir = std::path::Path::new("tests/inline");
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("tests/inline/ directory missing")
        .filter_map(|e| {
            let p = e.unwrap().path();
            p.extension().is_some_and(|e| e == "inline").then_some(p)
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no .inline files found in tests/inline/");

    let mut failures: Vec<String> = Vec::new();

    for path in &paths {
        let content = std::fs::read_to_string(path).unwrap();
        let file = path.file_name().unwrap().to_string_lossy();

        for case in parse_file(&content) {
            let label = format!("{file}:{}", case.line);
            let selection = Range::new(case.cursor, case.cursor);
            let doc = Document::new(case.source.clone());
            let (offered, resolved) = match case.which {
                Which::All => (
                    InlineAll::offer(&doc, selection).is_some(),
                    InlineAll::resolve(&doc, selection),
                ),
                Which::Here => (
                    InlineHere::offer(&doc, selection).is_some(),
                    InlineHere::resolve(&doc, selection),
                ),
            };

            match case.expected {
                None => {
                    if offered {
                        failures.push(format!("{label}: expected NONE but action was offered"));
                    }
                }
                Some(exp) => match (offered, resolved) {
                    (false, _) => {
                        failures.push(format!("{label}: expected the action to be offered"))
                    }
                    (true, None) => failures.push(format!("{label}: expected edits, got None")),
                    (true, Some(resolved)) => {
                        let actual = apply_edits(&case.source, &resolved.edits);
                        if actual.trim_end() != exp {
                            failures.push(format!(
                                "{label}: output mismatch\n--- expected ---\n{exp}\n--- actual ---\n{}",
                                actual.trim_end()
                            ));
                        }
                    }
                },
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "{} case(s) failed:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
}
