//! Prints the tree-sitter parse tree for a LilyPond snippet, as a debugging
//! aid when working out which node kinds and shapes the grammar produces.
//!
//! Reads the source from a file given as the first argument, or from stdin:
//!
//! ```text
//! cargo run --example dump_tree -- path/to/file.ly
//! echo '\lyricmode { la la }' | cargo run --example dump_tree
//! ```
//!
//! Each line shows a node's kind, its byte range, and — for leaf nodes — the
//! source it covers. Anonymous nodes (grammar literals like `{`) are shown in
//! quotes.

use std::io::Read;

use tree_sitter::{Node, Parser};

fn main() {
    let src = match std::env::args().nth(1) {
        Some(path) => {
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
        }
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .expect("read stdin");
            buf
        }
    };

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
        .expect("load LilyPond grammar");
    let tree = parser.parse(&src, None).expect("parser produces a tree");

    print_node(tree.root_node(), &src, 0);
}

fn print_node(node: Node<'_>, src: &str, depth: usize) {
    let indent = "  ".repeat(depth);
    let range = format!("{}..{}", node.start_byte(), node.end_byte());
    let label = if node.is_named() {
        node.kind().to_string()
    } else {
        format!("\"{}\"", node.kind())
    };

    if node.child_count() == 0 {
        let text = &src[node.start_byte()..node.end_byte()];
        println!("{indent}{label} [{range}] {:?}", truncate(text));
    } else {
        println!("{indent}{label} [{range}]");
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            print_node(child, src, depth + 1);
        }
    }
}

/// Collapses a leaf's source onto one readable line.
fn truncate(text: &str) -> String {
    const MAX: usize = 40;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        let head: String = text.chars().take(MAX).collect();
        format!("{head}…")
    }
}
