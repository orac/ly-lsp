//! Core logic for the LilyPond language server, kept separate from the binary so it can be exercised directly from tests.

// These docs are written to be read in the source, by whoever is next changing it, rather than published as an API reference.
#![allow(rustdoc::private_intra_doc_links)]

pub mod code_action;
pub mod command;
pub mod command_assist;
pub mod document;
pub mod document_graph;
pub mod line_struct;
pub mod note_analyser;
pub mod note_names;
pub mod notes;
pub mod semantic_tokens;
pub mod vocabulary;
