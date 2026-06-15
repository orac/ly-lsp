use ly_lsp::code_action::CodeAction;
use ly_lsp::code_action::make_explicit::{
    MakeBothExplicit, MakeDurationsExplicit, MakePitchesExplicit,
};
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

/// The three actions, each case is checked against all of them.
#[derive(Clone, Copy, PartialEq)]
enum Which {
    Durations,
    Pitches,
    Both,
}

impl Which {
    const ALL: [Which; 3] = [Which::Durations, Which::Pitches, Which::Both];

    /// The action word that introduces this action's expected section.
    fn word(self) -> &'static str {
        match self {
            Which::Durations => "dur",
            Which::Pitches => "pitch",
            Which::Both => "both",
        }
    }

    fn parse(word: &str) -> Self {
        match word {
            "dur" | "durations" => Which::Durations,
            "pitch" | "pitches" => Which::Pitches,
            "both" => Which::Both,
            other => panic!("unknown action `{other}` (use dur/pitch/both)"),
        }
    }

    /// Runs the action's offer and resolve over `doc`, returning whether it was
    /// offered and the document `src` resolves to (if it produced any edit).
    fn run(self, doc: &Document, src: &str, selection: Range) -> (bool, Option<String>) {
        let (offered, resolved) = match self {
            Which::Durations => (
                MakeDurationsExplicit::offer(doc, selection).is_some(),
                MakeDurationsExplicit::resolve(doc, selection),
            ),
            Which::Pitches => (
                MakePitchesExplicit::offer(doc, selection).is_some(),
                MakePitchesExplicit::resolve(doc, selection),
            ),
            Which::Both => (
                MakeBothExplicit::offer(doc, selection).is_some(),
                MakeBothExplicit::resolve(doc, selection),
            ),
        };
        let applied = resolved.map(|r| apply_edits(src, &r.edits));
        (offered, applied)
    }
}

/// One input with the output each of the three actions should produce. An
/// expected of `None` means the action should not be offered (the source `NONE`).
struct Case {
    source: String,
    selection: Range,
    durations: Option<String>,
    pitches: Option<String>,
    both: Option<String>,
    line: usize,
}

impl Case {
    fn expected(&self, which: Which) -> &Option<String> {
        match which {
            Which::Durations => &self.durations,
            Which::Pitches => &self.pitches,
            Which::Both => &self.both,
        }
    }
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

// A case is the annotated source, then one `--- <action>` section per action,
// each holding that action's expected output (or the word NONE). The selection
// is marked below its source line(s), as in the extract suite:
//
//   single line:  `   ^^^^`
//   multi line:    `   >>>` under the start line and `<<<` under the end.
//
// The leading spaces give the start column; the marker run gives the end column.
// Unlike the old format the marker carries no action word: every case is run
// through all three actions.
fn parse_case(text: &str, line: usize) -> Case {
    let mut lines = text.lines().peekable();

    let mut annotated: Vec<&str> = Vec::new();
    while let Some(&l) = lines.peek() {
        if action_header(l).is_some() {
            break;
        }
        annotated.push(lines.next().unwrap());
    }
    let (source, selection) = parse_selection(&annotated, text);

    let (mut durations, mut pitches, mut both) = (None, None, None);
    let (mut saw_dur, mut saw_pitch, mut saw_both) = (false, false, false);
    while let Some(header) = lines.next() {
        let which = action_header(header)
            .unwrap_or_else(|| panic!("expected an action header (`--- dur` etc.):\n{text}"));
        let mut body: Vec<&str> = Vec::new();
        while let Some(&l) = lines.peek() {
            if action_header(l).is_some() {
                break;
            }
            body.push(lines.next().unwrap());
        }
        let body = body.join("\n");
        let body = body.trim();
        let expected = (body != "NONE").then(|| body.to_string());
        match which {
            Which::Durations => (durations, saw_dur) = (expected, true),
            Which::Pitches => (pitches, saw_pitch) = (expected, true),
            Which::Both => (both, saw_both) = (expected, true),
        }
    }
    assert!(
        saw_dur && saw_pitch && saw_both,
        "case missing a `--- dur`/`--- pitch`/`--- both` section:\n{text}"
    );

    Case {
        source,
        selection,
        durations,
        pitches,
        both,
        line,
    }
}

/// An `--- <action>` section header, or `None` for any other line.
fn action_header(line: &str) -> Option<Which> {
    let rest = line.strip_prefix("---")?.trim();
    (!rest.is_empty()).then(|| Which::parse(rest))
}

/// Reads the selection markers interleaved with the source lines, returning the
/// source (markers stripped) and the marked range.
fn parse_selection(annotated: &[&str], text: &str) -> (String, Range) {
    let mut source_lines: Vec<&str> = Vec::new();
    let mut start_mark: Option<(usize, u32)> = None;
    let mut end_mark: Option<(usize, u32)> = None;

    for &line in annotated {
        let leading = (line.len() - line.trim_start().len()) as u32;
        let body = line.trim();
        let marker: String = body.chars().take_while(|c| "^><".contains(*c)).collect();
        if marker.is_empty() {
            source_lines.push(line);
            continue;
        }
        let row = source_lines.len().saturating_sub(1);
        let kind = marker.chars().next().unwrap();
        let len = marker.len() as u32;
        match kind {
            '^' => {
                start_mark = Some((row, leading));
                end_mark = Some((row, leading + len));
            }
            '>' => start_mark = Some((row, leading)),
            '<' => end_mark = Some((row, leading + len)),
            _ => unreachable!(),
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
    (source, selection)
}

#[test]
fn explicit_cases() {
    let dir = std::path::Path::new("tests/explicit");
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("tests/explicit/ directory missing")
        .filter_map(|e| {
            let p = e.unwrap().path();
            p.extension().is_some_and(|e| e == "explicit").then_some(p)
        })
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no .explicit files found in tests/explicit/"
    );

    let mut failures: Vec<String> = Vec::new();

    for path in &paths {
        let content = std::fs::read_to_string(path).unwrap();
        let file = path.file_name().unwrap().to_string_lossy();

        for case in parse_file(&content) {
            let doc = Document::new(case.source.clone());
            for which in Which::ALL {
                let label = format!("{file}:{} [{}]", case.line, which.word());
                let (offered, resolved) = which.run(&doc, &case.source, case.selection);
                match case.expected(which) {
                    // offer and resolve share the availability check, so an
                    // unoffered case must also resolve to nothing.
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
                        (true, Some(actual)) => {
                            if actual.trim_end() != *exp {
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
    }

    if !failures.is_empty() {
        panic!(
            "{} case(s) failed:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
}
