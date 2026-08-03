//! The graph of documents the server currently knows about, connected by their
//! `\include` directives, and the cross-file resolution built on it.
//!
//! Only *open* documents are indexed. Files reached solely through `\include`
//! are read from disk on demand to resolve a definition, but they are never
//! eagerly scanned: find-references reports only occurrences in files you have
//! open, even if a shared include is referenced from a hundred files on disk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use dashmap::DashMap;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionOrCommand, CompletionItem, Diagnostic, DocumentHighlight,
    DocumentHighlightKind, Hover, Location, Position, Range, SemanticToken, SignatureHelp,
    TextDocumentContentChangeEvent, TextEdit, Url, WorkspaceEdit,
};

use crate::document::Document;
use crate::vocabulary::{Scope, Vocabulary};

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

        // Computed *before* borrowing the open document below, so we never
        // re-enter the document map while holding a reference into it. The
        // scope answers for commands — builtins, and the functions this file
        // and its includes define — and `reachable` for every other kind of
        // definition an `\include` brings into view.
        let known = self.vocabulary.get().map(|_| {
            let scope = self.scope_for(uri);
            let reachable = self.reachable_definition_names(uri);
            move |name: &str| scope.is_known(name) || reachable.contains(name)
        });

        self.with_document(uri, |doc| {
            let mut diagnostics = doc.diagnostics();
            if let Some(known) = &known {
                diagnostics.extend(doc.undefined_reference_diagnostics(known));
            }
            diagnostics
        })
        .unwrap_or_default()
    }

    /// The names of every definition reachable from `uri` through includes.
    fn reachable_definition_names(&self, uri: &Url) -> HashSet<String> {
        let mut names = HashSet::new();
        for file in self.include_closure(uri) {
            self.with_document_raw(&file, |doc| {
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
                .with_document_raw(&file, |doc| doc.definition_ranges(name))
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
            self.with_document_raw(&current, |doc| {
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

    /// The code actions on offer for `range` in the document at `uri`, as
    /// unresolved actions whose edits are filled in later by
    /// [`resolve_code_action`](Self::resolve_code_action).
    pub fn code_actions(&self, uri: &Url, range: Range) -> Vec<CodeActionOrCommand> {
        self.with_document(uri, |doc| crate::code_action::offer_all(doc, uri, range))
            .unwrap_or_default()
    }

    /// Fills in the edits for a previously offered `action`, looking up the
    /// document it targets. Returns it unchanged if that document is gone.
    pub fn resolve_code_action(&self, action: CodeAction) -> CodeAction {
        let Some(uri) = crate::code_action::target_uri(&action) else {
            return action;
        };
        self.with_document(&uri, |doc| crate::code_action::resolve(doc, action.clone()))
            .unwrap_or(action)
    }

    /// Signature help for the command call at `position` in `uri`. See
    /// [`command_assist::signature_help`](crate::command_assist::signature_help).
    pub fn signature_help(&self, uri: &Url, position: Position) -> Option<SignatureHelp> {
        self.with_document(uri, |doc| {
            crate::command_assist::signature_help(doc, position)
        })
        .flatten()
    }

    /// Argument completions at `position` in `uri`. See
    /// [`command_assist::completions`](crate::command_assist::completions).
    pub fn completions(&self, uri: &Url, position: Position) -> Vec<CompletionItem> {
        self.with_document(uri, |doc| crate::command_assist::completions(doc, position))
            .unwrap_or_default()
    }

    /// Hover documentation for the command word at `position` in `uri`. See
    /// [`command_assist::hover`](crate::command_assist::hover).
    pub fn hover(&self, uri: &Url, position: Position) -> Option<Hover> {
        self.with_document(uri, |doc| crate::command_assist::hover(doc, position))
            .flatten()
    }

    /// Semantic tokens for the whole of the document at `uri`. See
    /// [`semantic_tokens::semantic_tokens_full`](crate::semantic_tokens::semantic_tokens_full).
    pub fn semantic_tokens_full(&self, uri: &Url) -> Vec<SemanticToken> {
        self.with_document(uri, crate::semantic_tokens::semantic_tokens_full)
            .unwrap_or_default()
    }

    /// Renames all definitions and references of the symbol at `position` in
    /// `uri` to `new_name`, across all open documents that share the same
    /// definition through their include closure.
    ///
    /// Returns `None` if the symbol has no user-defined definition (e.g. a
    /// built-in command), since renaming those is not meaningful.
    pub fn rename(&self, uri: &Url, position: Position, new_name: &str) -> Option<WorkspaceEdit> {
        let name = self.with_document(uri, |doc| doc.symbol_at(position).map(str::to_string))??;

        let definitions = self.definitions_of(&name, uri);
        if definitions.is_empty() {
            return None;
        }

        let anchors: HashSet<Url> = definitions.iter().map(|l| l.uri.clone()).collect();
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

        for loc in definitions {
            changes.entry(loc.uri).or_default().push(TextEdit {
                range: loc.range,
                new_text: new_name.to_string(),
            });
        }

        for open_uri in self.open_uris() {
            let closure: HashSet<Url> = self.include_closure(&open_uri).into_iter().collect();
            if closure.is_disjoint(&anchors) {
                continue;
            }
            let ranges = self
                .with_document(&open_uri, |doc| doc.reference_ranges(&name))
                .unwrap_or_default();
            for range in ranges {
                changes.entry(open_uri.clone()).or_default().push(TextEdit {
                    range,
                    new_text: format!("\\{new_name}"),
                });
            }
        }

        Some(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        })
    }

    /// Runs `f` against the document at `uri`, with its command analysis
    /// brought up to date for the [`Scope`] it can currently see.
    ///
    /// Everything a document derives from its own text alone is ready the
    /// moment it is parsed; only the command analysis depends on other files,
    /// so only that is refreshed here. See [`Document::refresh`].
    fn with_document<R>(&self, uri: &Url, f: impl FnOnce(&Document) -> R) -> Option<R> {
        // Built before any entry is borrowed: `scope_for` walks the include
        // graph, so computing it while holding a reference into the document
        // map would re-enter that map.
        let scope = self.scope_for(uri);
        self.with_document_raw(uri, |doc| {
            doc.refresh(&scope);
            f(doc)
        })
    }

    /// The commands visible from `uri`: the definitions of every file in its
    /// include closure, nearest first, over the global vocabulary.
    ///
    /// Each file contributes the layer its [`Document`] built when it was
    /// parsed, so a header included by twenty scores is read once and its
    /// layer shared twenty times over rather than re-read per score.
    fn scope_for(&self, uri: &Url) -> Scope<'_> {
        let mut layers = Vec::new();
        for file in self.include_closure(uri) {
            self.with_document_raw(&file, |doc| {
                let layer = doc.commands_defined();
                if !layer.is_empty() {
                    layers.push(Arc::clone(layer));
                }
            });
        }
        Scope::new(self.vocabulary.get(), layers)
    }

    /// Runs `f` against the document at `uri` without refreshing its
    /// cross-file analysis. Open documents are used directly; others are read
    /// from disk and cached, the cache being reused while the file's
    /// modification time is unchanged. Returns `None` if the document is
    /// neither open nor a readable file.
    ///
    /// This is what the include-graph walk itself uses — includes, symbols and
    /// a file's own definitions are all derived from the file alone, so asking
    /// for them can't need a scope, and mustn't, on pain of unbounded
    /// recursion through [`scope_for`](Self::scope_for).
    fn with_document_raw<R>(&self, uri: &Url, f: impl FnOnce(&mut Document) -> R) -> Option<R> {
        if let Some(mut doc) = self.open.get_mut(uri) {
            return Some(f(&mut doc));
        }

        let path = uri.to_file_path().ok()?;
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;

        if let Some(mut cached) = self.cache.get_mut(uri)
            && cached.modified == modified
        {
            return Some(f(&mut cached.document));
        }

        // Absent or stale: (re)read and parse, then cache.
        let mut document = Document::new(std::fs::read_to_string(&path).ok()?);
        let result = f(&mut document);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn url(path: &Path) -> Url {
        Url::from_file_path(path).expect("absolute path")
    }

    /// Two scores including one shared file of music functions. The scope each
    /// sees stacks the *same* layer instance, so their fingerprints match —
    /// the observable consequence of the shared file being read once rather
    /// than once per score that includes it.
    #[test]
    fn a_shared_include_is_read_once_for_every_file_that_includes_it() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared.ily");
        let one = dir.path().join("one.ly");
        let two = dir.path().join("two.ly");
        fs::write(
            &shared,
            "myFunc = #(define-music-function (m) (ly:music?) m)\n",
        )
        .unwrap();
        fs::write(&one, "\\include \"shared.ily\"\n\\myFunc { c4 }\n").unwrap();
        fs::write(&two, "\\include \"shared.ily\"\n\\myFunc { d4 }\n").unwrap();

        let ws = DocumentGraph::new();
        ws.open(url(&one), fs::read_to_string(&one).unwrap());
        ws.open(url(&two), fs::read_to_string(&two).unwrap());

        assert_eq!(
            ws.scope_for(&url(&one)).fingerprint(),
            ws.scope_for(&url(&two)).fingerprint(),
            "both scores should stack the same layer from the shared include"
        );
    }

    /// The other half of the same claim: a scope's fingerprint follows the
    /// content of the files in its closure, so an analysis made before an
    /// include was edited is not silently served afterwards.
    #[test]
    fn editing_an_include_changes_the_scope_of_the_files_that_include_it() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared.ily");
        let score = dir.path().join("score.ly");
        fs::write(
            &shared,
            "myFunc = #(define-music-function (m) (ly:music?) m)\n",
        )
        .unwrap();
        fs::write(&score, "\\include \"shared.ily\"\n\\myFunc { c4 }\n").unwrap();

        let ws = DocumentGraph::new();
        ws.open(url(&score), fs::read_to_string(&score).unwrap());
        let before = ws.scope_for(&url(&score)).fingerprint();

        // The editor opens the include and gives the function a second argument.
        ws.open(
            url(&shared),
            "myFunc = #(define-music-function (a m) (ly:pitch? ly:music?) m)\n".to_string(),
        );
        assert_ne!(before, ws.scope_for(&url(&score)).fingerprint());
    }

    /// Renaming reaches a definition bound inside `#( … )`, where the name is a
    /// bare `scheme_symbol` rather than an `assignment_lhs`. Worth checking on
    /// the resulting text rather than on the edits: a span off by the `#(` or
    /// by the leading backslash would corrupt the file.
    #[test]
    fn rename_rewrites_a_definition_bound_inside_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let score = dir.path().join("score.ly");
        let source =
            "#(define-public myFunc (define-music-function (m) (ly:music?) m))\n\\myFunc { c4 }\n";
        fs::write(&score, source).unwrap();

        let ws = DocumentGraph::new();
        ws.open(url(&score), source.to_string());

        // Cursor on the `\myFunc` call.
        let edit = ws
            .rename(&url(&score), Position::new(1, 2), "newFunc")
            .expect("a rename of a user-defined function");
        let mut edits = edit.changes.expect("changes")[&url(&score)].clone();
        // Apply back to front, so earlier offsets stay valid.
        edits.sort_by_key(|e| std::cmp::Reverse((e.range.start.line, e.range.start.character)));

        let index = crate::line_struct::LineIndex::new(source);
        let mut renamed = source.to_string();
        for TextEdit { range, new_text } in edits {
            let start = index.offset_at(range.start).expect("a start offset");
            let end = index.offset_at(range.end).expect("an end offset");
            renamed.replace_range(start..end, &new_text);
        }

        assert_eq!(
            renamed,
            "#(define-public newFunc (define-music-function (m) (ly:music?) m))\n\\newFunc { c4 }\n"
        );
    }

    /// A file that defines nothing and includes nothing sees the plain builtin
    /// scope — so the overwhelming majority of documents are analysed once,
    /// when they are parsed, and never re-analysed.
    #[test]
    fn a_file_with_no_definitions_stacks_no_layers() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.ly");
        fs::write(&plain, "{ c d e }\n").unwrap();

        let ws = DocumentGraph::new();
        ws.open(url(&plain), fs::read_to_string(&plain).unwrap());

        assert_eq!(
            ws.scope_for(&url(&plain)).fingerprint(),
            Scope::builtins_only().fingerprint()
        );
    }
}
