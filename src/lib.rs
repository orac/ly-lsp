//! Core logic for the LilyPond language server, kept separate from the binary
//! so it can be exercised directly from tests.

pub mod document;
pub mod document_graph;
pub mod line_struct;
pub mod note_names;
pub mod notes;
pub mod vocabulary;
