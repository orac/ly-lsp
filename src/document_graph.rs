//! The graph of documents the server currently knows about, connected by their
//! `\include` directives, and the cross-file resolution built on it.
//!
//! Only *open* documents are indexed. Files reached solely through `\include`
//! are read from disk on demand to resolve a definition, but they are never
//! eagerly scanned: find-references reports only occurrences in files you have
//! open, even if a shared include is referenced from a hundred files on disk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use dashmap::DashMap;
use tower_lsp::lsp_types::{
    Diagnostic, DocumentHighlight, DocumentHighlightKind, Location, Position, Range,
    TextDocumentContentChangeEvent, Url,
};

use crate::document::Document;
use crate::vocabulary::Vocabulary;

#[derive(Debug, Default)]
pub struct DocumentGraph {
    /// Documents currently open in the editor, keyed by URI.
    open: DashMap<Url, Document>,
    /// The commands LilyPond recognises, loaded once from its `lilypond-words`
    /// file. Unset until successfully loaded, which keeps undefined-reference
    /// diagnostics disabled rather than flagging every command when the words
    /// file is unavailable.
    vocabulary: OnceLock<Vocabulary>,
    /// Directories from LilyPond's `-I` option, searched (after the including
    /// file's own directory) when resolving `\include`.
    search_paths: OnceLock<Vec<PathBuf>>,
    /// Parsed documents for files reached only through `\include` (i.e. not
    /// open in the editor), so they aren't re-read and re-parsed on every
    /// query. Invalidated when the file's modification time changes.
    cache: DashMap<Url, CachedDocument>,
}

#[derive(Debug)]
struct CachedDocument {
    modified: SystemTime,
    document: Document,
}

impl DocumentGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, uri: Url, text: String) {
        // Once a file is open its live buffer supersedes any on-disk parse we
        // cached while it was merely an include; drop the now-shadowed entry.
        self.cache.remove(&uri);
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

    /// Loads the command vocabulary from LilyPond's `lilypond-words` file.
    ///
    /// Returns whether loading succeeded. On any failure (missing file, read
    /// error) the vocabulary stays unset and undefined-reference diagnostics
    /// remain off, so we never flag every command as undefined.
    pub fn load_vocabulary(&self, path: &Path) -> bool {
        match Vocabulary::load(path) {
            Some(vocabulary) => self.vocabulary.set(vocabulary).is_ok(),
            None => false,
        }
    }

    /// Sets the `-I` include search directories, in priority order.
    pub fn set_search_paths(&self, paths: Vec<PathBuf>) {
        let _ = self.search_paths.set(paths);
    }

    fn search_paths(&self) -> &[PathBuf] {
        self.search_paths.get().map_or(&[], Vec::as_slice)
    }

    /// Diagnostics for an open document: always its syntax errors, plus
    /// undefined-reference errors when the vocabulary has been loaded.
    pub fn diagnostics(&self, uri: &Url) -> Vec<Diagnostic> {
        if !self.open.contains_key(uri) {
            return Vec::new();
        }

        // Compute the reachable definition names *before* borrowing the open
        // document below, so we never re-enter the document map while holding a
        // reference into it.
        let vocabulary = self.vocabulary.get();
        let reachable = vocabulary.map(|_| self.reachable_definition_names(uri));

        self.with_document(uri, |doc| {
            let mut diagnostics = doc.diagnostics();
            if let (Some(vocabulary), Some(reachable)) = (vocabulary, &reachable) {
                diagnostics.extend(doc.undefined_reference_diagnostics(|name| {
                    vocabulary.is_known(name) || reachable.contains(name)
                }));
            }
            diagnostics
        })
        .unwrap_or_default()
    }

    /// The names of every definition reachable from `uri` through includes.
    fn reachable_definition_names(&self, uri: &Url) -> HashSet<String> {
        let mut names = HashSet::new();
        for file in self.include_closure(uri) {
            self.with_document(&file, |doc| {
                names.extend(doc.definitions().iter().map(|d| d.name.clone()));
            });
        }
        names
    }

    /// Document highlights for `position` in `uri`: matched bracket ranges, or
    /// all definitions/references of the symbol under the cursor, within the
    /// same document only.
    pub fn document_highlights(&self, uri: &Url, position: Position) -> Vec<DocumentHighlight> {
        if let Some(Some(pair)) = self.with_document(uri, |doc| doc.bracket_at(position)) {
            return pair
                .into_iter()
                .map(|range| DocumentHighlight {
                    range,
                    kind: Some(DocumentHighlightKind::TEXT),
                })
                .collect();
        }

        let Some(Some(name)) =
            self.with_document(uri, |doc| doc.symbol_at(position).map(str::to_string))
        else {
            return Vec::new();
        };

        let mut highlights = Vec::new();
        if let Some(ranges) = self.with_document(uri, |doc| doc.definition_ranges(&name)) {
            highlights.extend(ranges.into_iter().map(|range| DocumentHighlight {
                range,
                kind: Some(DocumentHighlightKind::WRITE),
            }));
        }
        if let Some(ranges) = self.with_document(uri, |doc| doc.reference_ranges(&name)) {
            highlights.extend(ranges.into_iter().map(|range| DocumentHighlight {
                range,
                kind: Some(DocumentHighlightKind::READ),
            }));
        }
        highlights
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
            return resolve_include(uri, &path, self.search_paths())
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
            locations.extend(
                ranges
                    .into_iter()
                    .map(|r| Location::new(open_uri.clone(), r)),
            );
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
                    if let Some(target) =
                        resolve_include(&current, &include.path, self.search_paths())
                    {
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

    /// Runs `f` against the document at `uri`. Open documents are used directly;
    /// others are read from disk and cached, the cache being reused while the
    /// file's modification time is unchanged. Returns `None` if the document is
    /// neither open nor a readable file.
    fn with_document<R>(&self, uri: &Url, f: impl FnOnce(&Document) -> R) -> Option<R> {
        if let Some(doc) = self.open.get(uri) {
            return Some(f(&doc));
        }

        let path = uri.to_file_path().ok()?;
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;

        if let Some(cached) = self.cache.get(uri)
            && cached.modified == modified
        {
            return Some(f(&cached.document));
        }

        // Absent or stale: (re)read and parse, then cache.
        let document = Document::new(std::fs::read_to_string(&path).ok()?);
        let result = f(&document);
        self.cache
            .insert(uri.clone(), CachedDocument { modified, document });
        Some(result)
    }
}

/// Resolves an `\include` path, searching the including file's own directory
/// first, then the `-I` search paths in order, and taking the first candidate
/// that exists. If none exist, falls back to the directory-relative path (so an
/// as-yet-uncreated include still has a sensible location).
///
/// Paths are joined but not canonicalised, so resolved URIs match what editors
/// send. LilyPond's current-working-directory search is not modelled.
fn resolve_include(base: &Url, path: &str, search_paths: &[PathBuf]) -> Option<Url> {
    let base_path = base.to_file_path().ok()?;
    let base_dir = base_path.parent()?;

    let existing = std::iter::once(base_dir)
        .chain(search_paths.iter().map(PathBuf::as_path))
        .map(|dir| dir.join(path))
        .find(|candidate| candidate.is_file());

    let resolved = existing.unwrap_or_else(|| base_dir.join(path));
    Url::from_file_path(resolved).ok()
}

/// The zero-width range at the start of a file.
fn start_of_file() -> Range {
    Range::new(Position::new(0, 0), Position::new(0, 0))
}
