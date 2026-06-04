use ly_lsp::code_action::CodeAction;
use ly_lsp::code_action::extract_to_variable::ExtractToVariable;
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

struct Case {
    source: String,
    selection: Range,
    expected: Option<String>,
}

fn parse_file(content: &str) -> Vec<Case> {
    content
        .split("\n===\n")
        .map(|b| b.trim_matches('\n'))
        .filter(|b| !b.is_empty())
        .map(parse_case)
        .collect()
}

// A case block contains the source with annotation lines interleaved, then
// `---`, then the expected output (or the word INVALID).
//
// Single-line selection: one `^` line immediately below the source line.
// Leading spaces give the start column; total length gives the end column.
//
// Multi-line selection: a `>` line below the start source line (leading spaces
// = start column) and a `<` line below the end source line (leading spaces +
// count = end column).
fn parse_case(text: &str) -> Case {
    let (annotated, expected_raw) = text
        .split_once("\n---\n")
        .unwrap_or_else(|| panic!("case missing '---' separator:\n{text}"));

    let mut source_lines: Vec<&str> = Vec::new();
    let mut start_mark: Option<(usize, u32)> = None;
    let mut end_mark: Option<(usize, u32)> = None;

    for line in annotated.lines() {
        let trimmed = line.trim();
        let leading = (line.len() - line.trim_start().len()) as u32;
        let row = source_lines.len().saturating_sub(1);
        if !trimmed.is_empty() && trimmed.chars().all(|c| c == '^') {
            start_mark = Some((row, leading));
            end_mark = Some((row, leading + trimmed.len() as u32));
        } else if !trimmed.is_empty() && trimmed.chars().all(|c| c == '>') {
            start_mark = Some((row, leading));
        } else if !trimmed.is_empty() && trimmed.chars().all(|c| c == '<') {
            end_mark = Some((row, leading + trimmed.len() as u32));
        } else {
            source_lines.push(line);
        }
    }

    let (start_row, start_col) =
        start_mark.unwrap_or_else(|| panic!("case missing start annotation:\n{text}"));
    let (end_row, end_col) =
        end_mark.unwrap_or_else(|| panic!("case missing end annotation:\n{text}"));
    let source = source_lines.join("\n") + "\n";
    let selection = Range::new(
        Position::new(start_row as u32, start_col),
        Position::new(end_row as u32, end_col),
    );
    let expected_str = expected_raw.trim();
    Case {
        source,
        selection,
        expected: (expected_str != "INVALID").then(|| expected_str.to_string()),
    }
}

#[test]
fn extract_cases() {
    let dir = std::path::Path::new("tests/extract");
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("tests/extract/ directory missing")
        .filter_map(|e| {
            let p = e.unwrap().path();
            p.extension().map_or(false, |e| e == "extract").then_some(p)
        })
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no .extract files found in tests/extract/"
    );

    for path in &paths {
        let content = std::fs::read_to_string(path).unwrap();
        let file = path.file_name().unwrap().to_string_lossy();

        for (i, case) in parse_file(&content).into_iter().enumerate() {
            let label = format!("{file}[{i}]");
            let doc = Document::new(case.source.clone());
            let offered = ExtractToVariable::offer(&doc, case.selection).is_some();

            match case.expected {
                // `offer` and `apply` share the validity check, so an invalid
                // selection must be neither offered nor applied — no dead menu
                // entries.
                None => assert!(!offered, "{label}: expected INVALID"),
                Some(exp) => {
                    assert!(offered, "{label}: expected the action to be offered");
                    let resolved = ExtractToVariable::resolve(&doc, case.selection)
                        .unwrap_or_else(|| panic!("{label}: expected edits, got None"));
                    let actual = apply_edits(&case.source, &resolved.edits);
                    assert_eq!(actual.trim_end(), exp, "{label}");
                }
            }
        }
    }
}
