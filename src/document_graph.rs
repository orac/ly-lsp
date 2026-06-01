//! The graph of documents the server currently knows about, connected by their
//! `\include` directives, and the cross-file resolution built on it.
//!
//! Only *open* documents are indexed. Files reached solely through `\include`
//! are read from disk on demand to resolve a definition, but they are never
//! eagerly scanned: find-references reports only occurrences in files you have
//! open, even if a shared include is referenced from a hundred files on disk.

use std::collections::HashSet;

use dashmap::DashMap;
use tower_lsp::lsp_types::{Location, Position, Range, TextDocumentContentChangeEvent, Url};

use crate::document::Document;

#[derive(Debug, Default)]
pub struct DocumentGraph {
    /// Documents currently open in the editor, keyed by URI.
    open: DashMap<Url, Document>,
}

impl DocumentGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, uri: Url, text: String) {
        self.open.insert(uri, Document::new(text));
    }

    pub fn close(&self, uri: &Url) {
        self.open.remove(uri);
    }

    /// Applies content changes to an open document, in order.
    pub fn change(&self, uri: &Url, changes: Vec<TextDocumentContentChangeEvent>) {
        if let Some(mut doc) = self.open.get_mut(uri) {
            for change in changes {
                doc.apply_change(change);
            }
        }
    }

    /// Resolves go-to-definition at `position` in document `uri`.
    ///
    /// If the cursor is on an `\include` path, the target file is returned.
    /// Otherwise the symbol under the cursor is resolved to its definition(s),
    /// searching the document and everything it includes (transitively).
    pub fn goto_definition(&self, uri: &Url, position: Position) -> Vec<Location> {
        // Include-path navigation takes precedence.
        if let Some(Some(path)) =
            self.with_document(uri, |doc| doc.include_at(position).map(str::to_string))
        {
            return resolve_include(uri, &path)
                .map(|target| vec![Location::new(target, start_of_file())])
                .unwrap_or_default();
        }

        let Some(Some(name)) =
            self.with_document(uri, |doc| doc.symbol_at(position).map(str::to_string))
        else {
            return Vec::new();
        };

        self.definitions_of(&name, uri)
    }

    /// Resolves find-references at `position` in document `uri`.
    ///
    /// References are collected only from *open* documents, and only from those
    /// whose include closure can see the definition the cursor resolves to — so
    /// unrelated files that happen to reuse the same name are not conflated, and
    /// files merely on disk are not scanned.
    pub fn references(
        &self,
        uri: &Url,
        position: Position,
        include_declaration: bool,
    ) -> Vec<Location> {
        let Some(Some(name)) =
            self.with_document(uri, |doc| doc.symbol_at(position).map(str::to_string))
        else {
            return Vec::new();
        };

        // The files that define this name, as the cursor sees it. If there's no
        // definition (e.g. a built-in), anchor on the cursor's own file so we
        // still report its references.
        let definitions = self.definitions_of(&name, uri);
        let anchors: HashSet<Url> = if definitions.is_empty() {
            std::iter::once(uri.clone()).collect()
        } else {
            definitions.iter().map(|loc| loc.uri.clone()).collect()
        };

        let mut locations = Vec::new();
        for open_uri in self.open_uris() {
            let closure: HashSet<Url> = self.include_closure(&open_uri).into_iter().collect();
            if closure.is_disjoint(&anchors) {
                continue;
            }
            let ranges = self
                .with_document(&open_uri, |doc| doc.reference_ranges(&name))
                .unwrap_or_default();
            locations.extend(ranges.into_iter().map(|r| Location::new(open_uri.clone(), r)));
        }

        if include_declaration {
            locations.extend(definitions);
        }
        locations
    }

    /// All definitions of `name` reachable from `uri` through includes.
    fn definitions_of(&self, name: &str, uri: &Url) -> Vec<Location> {
        let mut found = Vec::new();
        for file in self.include_closure(uri) {
            let ranges = self
                .with_document(&file, |doc| doc.definition_ranges(name))
                .unwrap_or_default();
            found.extend(ranges.into_iter().map(|r| Location::new(file.clone(), r)));
        }
        found
    }

    /// The set of files reachable from `uri` by following `\include` directives
    /// (including `uri` itself), in deterministic discovery order. Cycle-safe.
    fn include_closure(&self, uri: &Url) -> Vec<Url> {
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        let mut stack = vec![uri.clone()];

        while let Some(current) = stack.pop() {
            if !visited.insert(current.clone()) {
                continue;
            }
            order.push(current.clone());
            self.with_document(&current, |doc| {
                for include in doc.includes() {
                    if let Some(target) = resolve_include(&current, &include.path) {
                        stack.push(target);
                    }
                }
            });
        }
        order
    }

    /// The URIs of all currently open documents.
    fn open_uris(&self) -> Vec<Url> {
        self.open.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Runs `f` against the document at `uri`, whether it's open in the editor
    /// or read freshly from disk. Returns `None` if the document is neither
    /// open nor a readable file.
    fn with_document<R>(&self, uri: &Url, f: impl FnOnce(&Document) -> R) -> Option<R> {
        if let Some(doc) = self.open.get(uri) {
            return Some(f(&doc));
        }
        let path = uri.to_file_path().ok()?;
        let text = std::fs::read_to_string(path).ok()?;
        Some(f(&Document::new(text)))
    }
}

/// Resolves an `\include` path relative to the including file's directory.
///
/// LilyPond also consults `-I` search paths; that's not modelled yet. Paths are
/// joined but not canonicalised, so resolved URIs match what editors send for
/// files in the same directory tree.
fn resolve_include(base: &Url, path: &str) -> Option<Url> {
    let base_path = base.to_file_path().ok()?;
    let dir = base_path.parent()?;
    Url::from_file_path(dir.join(path)).ok()
}

/// The zero-width range at the start of a file.
fn start_of_file() -> Range {
    Range::new(Position::new(0, 0), Position::new(0, 0))
}
