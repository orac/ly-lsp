use ly_lsp::document::{Document, MusicExtractInfo};
use ly_lsp::line_struct::LineIndex;
use tower_lsp::lsp_types::{Position, Range};

// Applies the two extract edits to `src` using the fixed default name "music".
// The replace is applied first (it comes later in the text and doesn't shift
// the insert position, which is always <= the replace start).
fn apply_extract(src: &str, info: &MusicExtractInfo) -> String {
    let idx = LineIndex::new(src);
    let replace_start = idx.offset_at(info.replace_range.start).unwrap();
    let replace_end = idx.offset_at(info.replace_range.end).unwrap();
    let insert_pos = idx.offset_at(info.insert_before).unwrap();

    let mut out = src.to_string();
    out.replace_range(replace_start..replace_end, "\\music");
    out.insert_str(insert_pos, &format!("music = {}\n\n", info.music_text));
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

// A case block contains the source with `^` annotation lines interleaved, then
// `---`, then the expected output (or the word INVALID).  `^` lines consist
// entirely of `^` characters (plus leading spaces for column alignment) and
// annotate the source line immediately above them.
fn parse_case(text: &str) -> Case {
    let (annotated, expected_raw) = text
        .split_once("\n---\n")
        .unwrap_or_else(|| panic!("case missing '---' separator:\n{text}"));

    let mut source_lines: Vec<&str> = Vec::new();
    let mut annotation: Option<(usize, u32, u32)> = None;

    for line in annotated.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && trimmed.chars().all(|c| c == '^') {
            let leading = (line.len() - line.trim_start().len()) as u32;
            annotation = Some((
                source_lines.len().saturating_sub(1),
                leading,
                leading + trimmed.len() as u32,
            ));
        } else {
            source_lines.push(line);
        }
    }

    let (row, start_col, end_col) =
        annotation.unwrap_or_else(|| panic!("case missing '^' annotation:\n{text}"));
    let source = source_lines.join("\n") + "\n";
    let selection = Range::new(
        Position::new(row as u32, start_col),
        Position::new(row as u32, end_col),
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
    assert!(!paths.is_empty(), "no .extract files found in tests/extract/");

    for path in &paths {
        let content = std::fs::read_to_string(path).unwrap();
        let file = path.file_name().unwrap().to_string_lossy();

        for (i, case) in parse_file(&content).into_iter().enumerate() {
            let label = format!("{file}[{i}]");
            let doc = Document::new(case.source.clone());
            let result = doc.music_extract_info(case.selection);

            match case.expected {
                None => assert!(result.is_none(), "{label}: expected INVALID"),
                Some(exp) => {
                    let info = result
                        .unwrap_or_else(|| panic!("{label}: expected Some, got None"));
                    let actual = apply_extract(&case.source, &info);
                    assert_eq!(actual.trim_end(), exp, "{label}");
                }
            }
        }
    }
}
