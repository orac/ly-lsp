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

use crate::command::CompletionContext;
use crate::document::Document;
use crate::vocabulary::{self, Scope};

#[derive(Debug, Default)]
pub struct DocumentGraph {
    /// Documents currently open in the editor, keyed by URI.
    open: DashMap<Url, Document>,
    /// The layers every document in the workspace shares — LilyPond's words
    /// list, its install, and our curated signatures — loaded once from the
    /// install. Unset until successfully loaded, which keeps
    /// undefined-reference diagnostics disabled rather than flagging every
    /// command when the words file is unavailable.
    base: OnceLock<Scope>,
    /// The version of the installation [`base`](Self::base) was loaded from
    /// (`2.24.3`), for the commands whose completions depend on it — which is
    /// `\version`'s, and so far only `\version`'s. Kept beside the base rather
    /// than dug back out of it, since a layer's
    /// [`origin`](crate::vocabulary::Layer::origin) is a label to show a
    /// reader, not a field to parse.
    lilypond_version: OnceLock<String>,
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

/// Why [`DocumentGraph::load_vocabulary`] failed.
#[derive(Debug)]
pub enum LoadVocabularyError {
    /// The words file, or something else read while building the base scope,
    /// could not be read.
    Io(std::io::Error),
    /// The vocabulary was already loaded; a later call is rejected rather
    /// than silently overwriting the first (the underlying `OnceLock` only
    /// ever accepts one value).
    AlreadyLoaded,
}

impl std::fmt::Display for LoadVocabularyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadVocabularyError::Io(err) => write!(f, "{err}"),
            LoadVocabularyError::AlreadyLoaded => write!(f, "vocabulary was already loaded"),
        }
    }
}

impl std::error::Error for LoadVocabularyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadVocabularyError::Io(err) => Some(err),
            LoadVocabularyError::AlreadyLoaded => None,
        }
    }
}

impl From<std::io::Error> for LoadVocabularyError {
    fn from(err: std::io::Error) -> Self {
        LoadVocabularyError::Io(err)
    }
}

impl DocumentGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, uri: Url, text: String) {
        // Once a file is open its live buffer supersedes any on-disk parse we
        // cached while it was merely an include; drop the now-shadowed entry.
        self.cache.remove(&uri);
        let document = Document::named(file_name(&uri), text);
        self.open.insert(uri, document);
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

    /// Loads the command vocabulary from a LilyPond installation's
    /// version-specific share directory (the one holding `ly/` and
    /// `vim/syntax/`).
    ///
    /// On failure the vocabulary stays unset and undefined-reference
    /// diagnostics remain off, so we never flag every command as undefined;
    /// the returned error says why, for the caller to log.
    pub fn load_vocabulary(&self, share_dir: &Path) -> Result<(), LoadVocabularyError> {
        let base = vocabulary::workspace_base(share_dir)?;
        if let Some(version) = crate::install::version(share_dir) {
            let _ = self.lilypond_version.set(version.to_string());
        }
        self.base
            .set(base)
            .map_err(|_| LoadVocabularyError::AlreadyLoaded)
    }

    /// The layers under every document here: what was loaded from the
    /// installation, or our hand-written ones alone until that succeeds.
    fn base(&self) -> Scope {
        self.base.get().cloned().unwrap_or_else(Scope::builtins)
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
        // re-enter the document map while holding a reference into it. One
        // walk of the include graph answers for every kind of name: a
        // definition is a command in its file's layer, whether or not anything
        // says what arguments it takes.
        let known = self.base.get().map(|_| {
            let scope = self.scope_for(uri);
            move |name: &str| scope.is_known(name)
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

    /// Document highlights for `position` in `uri`: matched bracket ranges, or
    /// all definitions/references of the symbol under the cursor, within the
    /// same document only.
    ///
    /// A context type or instance is looked for before the ordinary symbol,
    /// the same order and for the same reason as in
    /// [`goto_definition`](Self::goto_definition) — each namespace is
    /// answered from its own tables, so highlighting `Staff` in `\new Staff`
    /// never lights up an unrelated `Staff = { … }` variable.
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

        let occurrences = self.with_document(uri, |doc| {
            if let Some(name) = doc.context_type_at(position) {
                return (
                    doc.context_type_declaration(name).into_iter().collect(),
                    doc.context_type_reference_ranges(name),
                );
            }
            if let Some(name) = doc.context_name_at(position) {
                return (
                    doc.context_instance_creation(name).into_iter().collect(),
                    doc.context_instance_reference_ranges(name),
                );
            }
            match doc.symbol_at(position) {
                Some(name) => (doc.definition_ranges(name), doc.reference_ranges(name)),
                None => (Vec::new(), Vec::new()),
            }
        });

        let Some((written, read)) = occurrences else {
            return Vec::new();
        };

        let highlight = |kind| {
            move |range| DocumentHighlight {
                range,
                kind: Some(kind),
            }
        };
        written
            .into_iter()
            .map(highlight(DocumentHighlightKind::WRITE))
            .chain(read.into_iter().map(highlight(DocumentHighlightKind::READ)))
            .collect()
    }

    /// Resolves go-to-definition at `position` in document `uri`.
    ///
    /// Tried in order: an `\include` path, under which the target file is
    /// returned; a context type or instance name, navigated through
    /// [`context_type_definitions`](Self::context_type_definitions) and
    /// [`context_instance_definitions`](Self::context_instance_definitions);
    /// and finally the ordinary symbol under the cursor, resolved to its
    /// definition(s) by searching the document and everything it includes
    /// (transitively). The first of the three that finds anything wins;
    /// none can overlap with another, since each looks at a different shape
    /// of node in the parse tree.
    pub fn goto_definition(&self, uri: &Url, position: Position) -> Vec<Location> {
        // Include-path navigation takes precedence.
        if let Some(Some(path)) =
            self.with_document(uri, |doc| doc.include_at(position).map(str::to_string))
        {
            return resolve_include(uri, &path, self.search_paths())
                .map(|target| vec![Location::new(target, start_of_file())])
                .unwrap_or_default();
        }

        if let Some(Some(name)) =
            self.with_document(uri, |doc| doc.context_type_at(position).map(str::to_string))
        {
            return self.context_type_definitions(&name, uri);
        }

        if let Some(Some(name)) =
            self.with_document(uri, |doc| doc.context_name_at(position).map(str::to_string))
        {
            return self.context_instance_definitions(&name, uri);
        }

        let Some((Some(name), at)) = self.with_document(uri, |doc| {
            (
                doc.symbol_at(position).map(str::to_string),
                doc.line_index().offset_at(position),
            )
        }) else {
            return Vec::new();
        };

        self.definitions_in_effect(&name, uri, at)
    }

    /// One [`Location`] per file in `uri`'s include closure that declares a
    /// context type called `name` — the context-type counterpart of
    /// [`definitions_in_effect`](Self::definitions_in_effect), which
    /// go-to-definition uses for the command namespace.
    ///
    /// Uses [`with_document_raw`](Self::with_document_raw) rather than
    /// [`with_document`](Self::with_document): a `\context { \name … }`
    /// declaration is read from a file's own parse tree alone
    /// ([`Document::context_type_declaration`]), not from anything that
    /// depends on the include graph, exactly like
    /// [`Document::definition_ranges`] before it.
    ///
    /// There is no cursor position to resolve against, unlike
    /// `definitions_in_effect`: a [`ContextType`](crate::context::ContextType)
    /// carries no redefinition chain, so at most one location comes back per
    /// file. Two files in the closure both declaring `name` is the same
    /// "genuine ambiguity" `definitions_in_effect` leaves for the reader to
    /// settle — both are offered, as before. A type known only from the
    /// LilyPond install yields nothing here at all: install-layer context
    /// types belong to no [`Document`] in this graph for
    /// [`context_type_declaration`](Document::context_type_declaration) to
    /// find, which is the documented gap `doc/command-parsing.md` already
    /// records ("go-to-definition into the install") rather than a new one.
    fn context_type_definitions(&self, name: &str, uri: &Url) -> Vec<Location> {
        self.include_closure(uri)
            .into_iter()
            .filter_map(|file| {
                let range = self
                    .with_document_raw(&file, |doc| doc.context_type_declaration(name))
                    .flatten()?;
                Some(Location::new(file, range))
            })
            .collect()
    }

    /// One [`Location`] per file in `uri`'s include closure that creates a
    /// context instance called `name` — the
    /// [`ContextInstance`](crate::context::ContextInstance) counterpart of
    /// [`context_type_definitions`](Self::context_type_definitions), reading
    /// [`Document::context_instance_creation`] the same way.
    fn context_instance_definitions(&self, name: &str, uri: &Url) -> Vec<Location> {
        self.include_closure(uri)
            .into_iter()
            .filter_map(|file| {
                let range = self
                    .with_document_raw(&file, |doc| doc.context_instance_creation(name))
                    .flatten()?;
                Some(Location::new(file, range))
            })
            .collect()
    }

    /// The definition of `name` a reference at byte offset `at` in `uri`
    /// actually means — one per file in the include closure that defines it,
    /// rather than every place each of them binds the name.
    ///
    /// See [`Document::definition_in_effect`] for what "in effect" means within
    /// a file. Across files it stays unresolved: two files in one closure both
    /// defining `foo` is a genuine ambiguity we don't have the information to
    /// settle here, since the closure walk doesn't record where each `\include`
    /// sits relative to the reference. Both are offered, as before.
    fn definitions_in_effect(&self, name: &str, uri: &Url, at: Option<usize>) -> Vec<Location> {
        let mut found = Vec::new();
        for file in self.include_closure(uri) {
            // Only the cursor's own file has a position to resolve against.
            let at = if file == *uri { at } else { None };
            let range = self
                .with_document_raw(&file, |doc| doc.definition_in_effect(name, at))
                .flatten();
            found.extend(range.map(|range| Location::new(file.clone(), range)));
        }
        found
    }

    /// Resolves find-references at `position` in document `uri`.
    ///
    /// References are collected only from *open* documents, and only from those
    /// whose include closure can see the definition the cursor resolves to — so
    /// unrelated files that happen to reuse the same name are not conflated, and
    /// files merely on disk are not scanned.
    /// Tried in the same order as [`goto_definition`](Self::goto_definition),
    /// minus the `\include` path (a file name is nobody's symbol): a context
    /// type, a context instance, then the ordinary symbol under the cursor.
    /// Each namespace answers for itself, so `\new MyStaff` and a variable
    /// that happens to be called `MyStaff` are never conflated.
    pub fn references(
        &self,
        uri: &Url,
        position: Position,
        include_declaration: bool,
    ) -> Vec<Location> {
        if let Some(Some(name)) =
            self.with_document(uri, |doc| doc.context_type_at(position).map(str::to_string))
        {
            let definitions = self.context_type_definitions(&name, uri);
            return self.gather_references(uri, definitions, include_declaration, |doc| {
                doc.context_type_reference_ranges(&name)
            });
        }

        if let Some(Some(name)) =
            self.with_document(uri, |doc| doc.context_name_at(position).map(str::to_string))
        {
            let definitions = self.context_instance_definitions(&name, uri);
            return self.gather_references(uri, definitions, include_declaration, |doc| {
                doc.context_instance_reference_ranges(&name)
            });
        }

        let Some(Some(name)) =
            self.with_document(uri, |doc| doc.symbol_at(position).map(str::to_string))
        else {
            return Vec::new();
        };

        let definitions = self.definitions_of(&name, uri);
        self.gather_references(uri, definitions, include_declaration, |doc| {
            doc.reference_ranges(&name)
        })
    }

    /// The find-references answer for a name whose `definitions` have already
    /// been resolved in whichever namespace it belongs to: every range
    /// `ranges` reads off an open document that can see one of those
    /// definitions, plus the definitions themselves where the client asked
    /// for them.
    ///
    /// The anchoring rule lives here, once, rather than in each namespace's
    /// caller: references come only from *open* documents whose include
    /// closure reaches a file that defines the name, so two unrelated files
    /// reusing a spelling are never conflated and files merely on disk are
    /// never scanned. Where there is no definition at all — a built-in
    /// command, or a context type the install declares and no
    /// [`Location`] can point at — the cursor's own file anchors instead, so
    /// its references are still reported.
    fn gather_references(
        &self,
        uri: &Url,
        definitions: Vec<Location>,
        include_declaration: bool,
        ranges: impl Fn(&Document) -> Vec<Range>,
    ) -> Vec<Location> {
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
            let found = self.with_document(&open_uri, &ranges).unwrap_or_default();
            locations.extend(
                found
                    .into_iter()
                    .map(|r| Location::new(open_uri.clone(), r)),
            );
        }

        if include_declaration {
            locations.extend(definitions);
        }
        locations
    }

    /// *Every* place `name` is bound in the files reachable from `uri` — not
    /// only the definition in effect at any particular point.
    ///
    /// Deliberately different from [`definitions_in_effect`](Self::definitions_in_effect),
    /// which go-to-definition uses. Rename and find-references treat a name as
    /// one thing across a file: renaming `\foo` where a second definition is in
    /// effect but leaving the first alone would rewrite the references that
    /// meant the first one too, and break them. Renaming the name entire is
    /// self-consistent; resolving each reference to its own binding first is a
    /// scoped rename, and a bigger job than this.
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

    /// Completions at `position` in `uri` — command names or a closed-set
    /// argument, depending on where the cursor is. See
    /// [`command_assist::completions`](crate::command_assist::completions).
    pub fn completions(&self, uri: &Url, position: Position) -> Vec<CompletionItem> {
        // Built inside the closure, not before it like other read-only
        // fields would be: `CompletionContext::scope` must be this
        // document's own scope, which only exists once `with_document` has
        // refreshed it — see `Document::scope`.
        self.with_document(uri, |doc| {
            let ctx = CompletionContext {
                lilypond_version: self.lilypond_version.get().map(String::as_str),
                scope: doc.scope(),
            };
            crate::command_assist::completions(doc, position, &ctx)
        })
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
    fn scope_for(&self, uri: &Url) -> Scope {
        let mut layers = Vec::new();
        for file in self.include_closure(uri) {
            self.with_document_raw(&file, |doc| {
                layers.push(Arc::clone(doc.commands_defined()));
            });
        }
        self.base().for_document(&layers)
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
        let mut document = Document::named(file_name(uri), std::fs::read_to_string(&path).ok()?);
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

/// What to call the document at `uri` when hover says where a command came
/// from: the last segment of the path, `parts/violin.ly` becoming `violin.ly`.
///
/// Two includes with the same file name are told apart only by hovering the
/// second one, which is a price worth paying for a line short enough to read:
/// where the command *is* remains go-to-definition's answer, not hover's.
/// Taken through the file path rather than off the URI's last segment, so that
/// the name reads as it does on disk (`my%20score.ly` is `my score.ly`). A URI
/// that names no file — an unsaved buffer's `untitled:` — falls back to the
/// whole of it rather than to nothing.
fn file_name(uri: &Url) -> String {
    uri.to_file_path()
        .ok()
        .as_deref()
        .and_then(Path::file_name)
        .map_or_else(
            || uri.to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
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

    #[test]
    fn a_document_is_named_for_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let score = dir.path().join("my score.ly");
        let graph = DocumentGraph::new();
        graph.open(url(&score), "foo = { c }\n".to_string());
        let named = graph.with_document(&url(&score), |document| {
            document.commands_defined().origin().to_string()
        });
        assert_eq!(named.as_deref(), Some("my score.ly"));
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

    /// A context instance created in an included file is visible from the
    /// including document — the include-closure counterpart of
    /// `Document::a_document_exposes_a_context_instance_its_own_music_creates`,
    /// which only checks a file seeing its own.
    #[test]
    fn an_included_files_context_instance_is_visible_to_the_including_document() {
        let dir = tempfile::tempdir().unwrap();
        let voices = dir.path().join("voices.ily");
        let score = dir.path().join("score.ly");
        fs::write(&voices, "melody = { \\new Voice = \"vocals\" { c } }\n").unwrap();
        fs::write(
            &score,
            "\\include \"voices.ily\"\n{ \\melody \\lyricsto \"vocals\" { la } }\n",
        )
        .unwrap();

        let ws = DocumentGraph::new();
        ws.open(url(&score), fs::read_to_string(&score).unwrap());

        let scope = ws.scope_for(&url(&score));
        let known = scope
            .get_context_instance("vocals")
            .expect("vocals, created in the included file");
        assert_eq!(known.value.type_name.as_deref(), Some("Voice"));
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

    /// The position a byte inside `needle`'s first occurrence in `src` maps
    /// to — enough of a cursor to land inside the name being navigated from,
    /// without having to hand-count characters in each test's source.
    fn inside(src: &str, needle: &str) -> Position {
        let offset = src.find(needle).expect("needle must occur in src") + 1;
        crate::line_struct::LineIndex::new(src).position_at(offset)
    }

    /// The same as [`inside`], but searching for `needle` only after
    /// `anchor`'s first occurrence — for a source with two occurrences of the
    /// same name (a context instance's creation and a later reference to it),
    /// where the cursor belongs on the second.
    fn inside_after(src: &str, anchor: &str, needle: &str) -> Position {
        let from = src.find(anchor).expect("anchor must occur in src");
        let offset = from + src[from..].find(needle).expect("needle after anchor") + 1;
        crate::line_struct::LineIndex::new(src).position_at(offset)
    }

    #[test]
    fn goto_definition_from_new_finds_a_context_type_declared_in_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let score = dir.path().join("score.ly");
        let src = "\\layout { \\context { \\name MyStaff } }\n{ \\new MyStaff { c } }\n";

        let ws = DocumentGraph::new();
        ws.open(url(&score), src.to_string());

        let locations = ws.goto_definition(&url(&score), inside(src, "MyStaff {"));
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].uri, url(&score));
        let start = crate::line_struct::LineIndex::new(src)
            .offset_at(locations[0].range.start)
            .unwrap();
        let end = crate::line_struct::LineIndex::new(src)
            .offset_at(locations[0].range.end)
            .unwrap();
        assert_eq!(&src[start..end], "MyStaff");
        // Landed on the `\name`'s declaration, not the `\new`'s reference.
        assert!(start < src.find("{ \\new").unwrap());
    }

    #[test]
    fn goto_definition_from_new_finds_a_context_type_declared_in_an_included_file() {
        let dir = tempfile::tempdir().unwrap();
        let types = dir.path().join("types.ily");
        let score = dir.path().join("score.ly");
        fs::write(&types, "\\layout { \\context { \\name MyStaff } }\n").unwrap();
        let src = "\\include \"types.ily\"\n{ \\new MyStaff { c } }\n";
        fs::write(&score, src).unwrap();

        let ws = DocumentGraph::new();
        ws.open(url(&score), src.to_string());

        let locations = ws.goto_definition(&url(&score), inside(src, "MyStaff {"));
        assert_eq!(locations.len(), 1);
        assert_eq!(
            locations[0].uri,
            url(&types),
            "the declaration lives in the included file, not the score"
        );
    }

    #[test]
    fn goto_definition_from_lyricsto_finds_the_new_that_created_the_instance() {
        let dir = tempfile::tempdir().unwrap();
        let score = dir.path().join("score.ly");
        let src = "{ \\new Voice = \"vocals\" { c } \\lyricsto \"vocals\" { la } }\n";

        let ws = DocumentGraph::new();
        ws.open(url(&score), src.to_string());

        let locations = ws.goto_definition(&url(&score), inside_after(src, "\\lyricsto", "vocals"));
        assert_eq!(locations.len(), 1);
        let start = crate::line_struct::LineIndex::new(src)
            .offset_at(locations[0].range.start)
            .unwrap();
        let end = crate::line_struct::LineIndex::new(src)
            .offset_at(locations[0].range.end)
            .unwrap();
        assert_eq!(&src[start..end], "vocals");
        // Landed on the `\new`'s creation, before `\lyricsto`'s own reference.
        assert!(start < src.find("\\lyricsto").unwrap());
    }

    /// The source text `locations` cover, in the order they came back.
    fn covered<'a>(src: &'a str, locations: &[Location]) -> Vec<&'a str> {
        let lines = crate::line_struct::LineIndex::new(src);
        locations
            .iter()
            .map(|location| {
                let start = lines.offset_at(location.range.start).unwrap();
                let end = lines.offset_at(location.range.end).unwrap();
                &src[start..end]
            })
            .collect()
    }

    #[test]
    fn find_references_on_a_context_type_finds_every_new_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let score = dir.path().join("score.ly");
        let src = "\\layout { \\context { \\name MyStaff } }\n{ \\new MyStaff { c } \\context MyStaff { d } }\n";

        let ws = DocumentGraph::new();
        ws.open(url(&score), src.to_string());

        let locations = ws.references(&url(&score), inside(src, "MyStaff {"), false);
        assert_eq!(covered(src, &locations), vec!["MyStaff", "MyStaff"]);

        // With the declaration asked for, the `\name` joins them.
        let with_declaration = ws.references(&url(&score), inside(src, "MyStaff {"), true);
        assert_eq!(with_declaration.len(), 3);
    }

    #[test]
    fn find_references_on_a_context_type_reaches_an_including_file() {
        // The include closure rule find-references already follows for
        // commands: the score can see the declaration in the header, so its
        // own `\new` counts as a reference to it.
        let dir = tempfile::tempdir().unwrap();
        let types = dir.path().join("types.ily");
        let score = dir.path().join("score.ly");
        fs::write(&types, "\\layout { \\context { \\name MyStaff } }\n").unwrap();
        let src = "\\include \"types.ily\"\n{ \\new MyStaff { c } }\n";
        fs::write(&score, src).unwrap();

        let ws = DocumentGraph::new();
        ws.open(url(&score), src.to_string());

        let locations = ws.references(&url(&score), inside(src, "MyStaff {"), false);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].uri, url(&score));
    }

    #[test]
    fn find_references_on_a_context_instance_skips_the_new_that_created_it() {
        let dir = tempfile::tempdir().unwrap();
        let score = dir.path().join("score.ly");
        let src = "{ \\new Voice = \"vocals\" { c } \\lyricsto \"vocals\" { la } \\change Voice = \"vocals\" }\n";

        let ws = DocumentGraph::new();
        ws.open(url(&score), src.to_string());

        let at = inside_after(src, "\\lyricsto", "vocals");
        let locations = ws.references(&url(&score), at, false);
        assert_eq!(
            covered(src, &locations),
            vec!["vocals", "vocals"],
            "the `\\lyricsto` and the `\\change`, not the `\\new` that created it"
        );
        for location in &locations {
            let start = crate::line_struct::LineIndex::new(src)
                .offset_at(location.range.start)
                .unwrap();
            assert!(start > src.find("\\lyricsto").unwrap());
        }

        assert_eq!(
            ws.references(&url(&score), at, true).len(),
            3,
            "the creation comes back as the declaration instead"
        );
    }

    #[test]
    fn a_variable_of_the_same_name_is_not_confused_with_a_context_type() {
        // Separate namespaces: `MyStaff = { … }` binds a command, and the
        // `\new MyStaff` names a context type. Neither should report the
        // other's occurrences.
        let dir = tempfile::tempdir().unwrap();
        let score = dir.path().join("score.ly");
        let src = "MyStaff = { c }\n\\layout { \\context { \\name MyStaff } }\n{ \\new MyStaff { \\MyStaff } }\n";

        let ws = DocumentGraph::new();
        ws.open(url(&score), src.to_string());

        let from_context = ws.references(&url(&score), inside(src, "MyStaff {"), false);
        assert_eq!(covered(src, &from_context), vec!["MyStaff"]);
        let start = crate::line_struct::LineIndex::new(src)
            .offset_at(from_context[0].range.start)
            .unwrap();
        assert_eq!(start, src.find("\\new MyStaff").unwrap() + "\\new ".len());

        let from_command = ws.references(&url(&score), inside(src, "\\MyStaff"), false);
        assert_eq!(covered(src, &from_command), vec!["\\MyStaff"]);
    }

    #[test]
    fn highlighting_a_context_type_marks_its_declaration_and_its_uses() {
        let dir = tempfile::tempdir().unwrap();
        let score = dir.path().join("score.ly");
        let src = "\\layout { \\context { \\name MyStaff } }\n{ \\new MyStaff { c } }\n";

        let ws = DocumentGraph::new();
        ws.open(url(&score), src.to_string());

        let highlights = ws.document_highlights(&url(&score), inside(src, "MyStaff {"));
        let kinds: Vec<_> = highlights.iter().map(|h| h.kind).collect();
        assert_eq!(
            kinds,
            vec![
                Some(DocumentHighlightKind::WRITE),
                Some(DocumentHighlightKind::READ)
            ]
        );
    }

    #[test]
    fn highlighting_a_context_instance_covers_each_name_once() {
        // The `\new`'s own name is the WRITE; without the creation being
        // excluded from the references it would also come back as a READ,
        // and the two ranges would overlap at different widths.
        let dir = tempfile::tempdir().unwrap();
        let score = dir.path().join("score.ly");
        let src = "{ \\new Voice = \"vocals\" { c } \\lyricsto \"vocals\" { la } }\n";

        let ws = DocumentGraph::new();
        ws.open(url(&score), src.to_string());

        let highlights =
            ws.document_highlights(&url(&score), inside_after(src, "\\lyricsto", "vocals"));
        assert_eq!(highlights.len(), 2);
        let mut ranges: Vec<_> = highlights.iter().map(|h| h.range).collect();
        ranges.dedup();
        assert_eq!(ranges.len(), 2, "the two occurrences are distinct places");
        let locations: Vec<Location> = highlights
            .iter()
            .map(|h| Location::new(url(&score), h.range))
            .collect();
        assert_eq!(covered(src, &locations), vec!["vocals", "vocals"]);
    }

    #[test]
    fn goto_definition_on_a_builtin_context_type_finds_nothing() {
        // `Staff` is never declared with `\name` anywhere in this workspace —
        // the shape a built-in type looks like from here, since install-layer
        // types carry no file for a `Location` to point at (the documented
        // gap in doc/command-parsing.md). Nothing is the right answer, not a
        // location in the wrong file.
        let dir = tempfile::tempdir().unwrap();
        let score = dir.path().join("score.ly");
        let src = "{ \\new Staff { c } }\n";

        let ws = DocumentGraph::new();
        ws.open(url(&score), src.to_string());

        assert!(
            ws.goto_definition(&url(&score), inside(src, "Staff {"))
                .is_empty()
        );
    }
}
