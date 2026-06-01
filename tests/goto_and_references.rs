//! End-to-end exercise of the analysis API on a realistic snippet, mirroring
//! what the go-to-definition and find-references handlers do.

use ly_lsp::document::Document;
use tower_lsp::lsp_types::{Position, Range};

const SOURCE: &str = "\
melody = \\relative c' {
  c4 d e f
}

\\score {
  \\new Staff {
    \\melody
    \\melody
  }
}
";

/// Returns a `Document` over the shared snippet.
fn doc() -> Document {
    Document::new(SOURCE.to_string())
}

#[test]
fn goto_definition_from_reference() {
    let doc = doc();
    // Cursor on the first `\melody` reference (line 6, inside the word).
    let name = doc.symbol_at(Position::new(6, 6)).expect("symbol under cursor");
    assert_eq!(name, "melody");

    let defs = doc.definition_ranges(name);
    assert_eq!(
        defs,
        vec![Range::new(Position::new(0, 0), Position::new(0, 6))]
    );
}

#[test]
fn builtin_command_has_no_definition() {
    let doc = doc();
    // `\relative` is a built-in; the cursor finds it but it resolves nowhere.
    let name = doc.symbol_at(Position::new(0, 12)).expect("symbol under cursor");
    assert_eq!(name, "relative");
    assert!(doc.definition_ranges(name).is_empty());
}

#[test]
fn find_all_references_to_variable() {
    let doc = doc();
    // Starting from the definition, we find both `\melody` uses.
    let name = doc.symbol_at(Position::new(0, 0)).expect("symbol under cursor");
    let refs = doc.reference_ranges(name);
    assert_eq!(
        refs,
        vec![
            Range::new(Position::new(6, 4), Position::new(6, 11)),
            Range::new(Position::new(7, 4), Position::new(7, 11)),
        ]
    );
}
