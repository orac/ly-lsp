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

// Renders `source` with the selected region highlighted using ANSI underline.
// Positions are in LSP (line, UTF-16 char) units; test files are ASCII so
// char counts equal byte counts.
fn render_with_underline(source: &str, sel: Range) -> String {
    const UL: &str = "\x1b[4m";
    const RESET: &str = "\x1b[0m";

    let s_line = sel.start.line as usize;
    let s_col = sel.start.character as usize;
    let e_line = sel.end.line as usize;
    let e_col = sel.end.character as usize;

    source
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i < s_line || i > e_line {
                return format!("{line}\n");
            }
            let n = line.chars().count();
            let from = if i == s_line { s_col.min(n) } else { 0 };
            let to = if i == e_line { e_col.min(n) } else { n };
            let before: String = line.chars().take(from).collect();
            let mid: String = line
                .chars()
                .skip(from)
                .take(to.saturating_sub(from))
                .collect();
            let after: String = line.chars().skip(to).collect();
            if mid.is_empty() {
                format!("{before}{after}\n")
            } else {
                format!("{before}{UL}{mid}{RESET}{after}\n")
            }
        })
        .collect()
}

struct Case {
    source: String,
    selection: Range,
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
        // newlines in block + 1 for the content line that had no trailing \n
        // + 1 for the === separator line
        current_line += block.bytes().filter(|&b| b == b'\n').count() + 2;
    }
    cases
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
fn parse_case(text: &str, line: usize) -> Case {
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
        line,
    }
}

#[test]
fn extract_cases() {
    let dir = std::path::Path::new("tests/extract");
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("tests/extract/ directory missing")
        .filter_map(|e| {
            let p = e.unwrap().path();
            p.extension().is_some_and(|e| e == "extract").then_some(p)
        })
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no .extract files found in tests/extract/"
    );

    let mut failures: Vec<String> = Vec::new();

    for path in &paths {
        let content = std::fs::read_to_string(path).unwrap();
        let file = path.file_name().unwrap().to_string_lossy();

        for case in parse_file(&content) {
            let label = format!("{file}:{}", case.line);
            let rendered = render_with_underline(&case.source, case.selection);
            let doc = Document::new(case.source.clone());
            let offered = ExtractToVariable::offer(&doc, case.selection).is_some();

            match case.expected {
                // `offer` and `apply` share the validity check, so an invalid
                // selection must be neither offered nor applied — no dead menu
                // entries.
                None => {
                    if offered {
                        failures.push(format!(
                            "{label}: expected INVALID but action was offered\n{rendered}"
                        ));
                    }
                }
                Some(exp) => {
                    if !offered {
                        failures.push(format!(
                            "{label}: expected the action to be offered but it wasn't\n{rendered}"
                        ));
                        continue;
                    }
                    match ExtractToVariable::resolve(&doc, case.selection) {
                        None => {
                            failures.push(format!("{label}: expected edits, got None\n{rendered}"))
                        }
                        Some(resolved) => {
                            let actual = apply_edits(&case.source, &resolved.edits);
                            if actual.trim_end() != exp {
                                failures.push(format!(
                                    "{label}: output mismatch\n{rendered}\n--- expected ---\n{exp}\n--- actual ---\n{}",
                                    actual.trim_end()
                                ));
                            }
                        }
                    }
                }
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
