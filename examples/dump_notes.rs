//! Prints the lexically resolved note state for a LilyPond file, as a debugging
//! aid for the analysis in `src/notes.rs`.
//!
//! ```text
//! cargo run --example dump_notes -- path/to/file.ly
//! echo "\relative c' { c d8 e }" | cargo run --example dump_notes
//! ```
//!
//! Each event is shown with its resolved pitch(es) and duration; lines flagged
//! `INVALID` are bare symbols that matched no note name in the active language.

use std::io::Read;

use ly_lsp::notes::{self, EventKind};

use tree_sitter::Parser;

fn main() {
    let src = match std::env::args().nth(1) {
        Some(path) => std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {path}: {e}")),
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).expect("read stdin");
            buf
        }
    };

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
        .expect("load LilyPond grammar");
    let tree = parser.parse(&src, None).expect("parser produces a tree");
    let analysis = notes::analyse(&tree, &src);

    for event in &analysis.events {
        let text = &src[event.span.start..event.span.end];
        let inherited = if event.duration_written { "" } else { " (inherited)" };
        let kind = match &event.kind {
            EventKind::Note { pitch, .. } => format!("note {pitch:?}"),
            EventKind::Chord(notes) => format!("chord {:?}", pitch_list(notes)),
            EventKind::ChordRepetition(notes) => format!("q {:?}", pitch_list(notes)),
            EventKind::Rest => "rest".to_string(),
            EventKind::MultiMeasureRest => "multi-measure rest".to_string(),
            EventKind::Skip => "skip".to_string(),
        };
        println!("{text:<16} {kind}  dur {}{inherited}", event.duration);
    }

    for problem in &analysis.problems {
        let span = problem.span();
        println!("PROBLEM  {problem:?}  {:?}", &src[span.start..span.end]);
    }
}

fn pitch_list(notes: &[notes::ChordNote]) -> Vec<(u8, i32, i8)> {
    notes
        .iter()
        .map(|n| (n.pitch.note_name, n.pitch.octave, n.pitch.alteration))
        .collect()
}
