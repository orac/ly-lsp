//! Per-document analysis: parses source with the tree-sitter LilyPond grammar,
//! extracts the symbols (definitions and references) we answer
//! go-to-definition and find-references with, and converts between byte offsets
//! and LSP positions.
//!
//! Definitions come from two readers, because LilyPond has two ways of binding
//! a name that `\foo` can reach. [`SYMBOL_QUERY`] captures the ordinary
//! assignment, `foo = …`, from its left-hand side. [`command::scheme`] captures
//! the Scheme binding forms, `#(define-public foo …)`, which produce no
//! `assignment_lhs` for a query to match on. [`merge_bindings`] puts the two in
//! source order, and the result is the file's
//! [`Layer`]: a definition is a command that takes no
//! arguments unless something says otherwise, so one table answers both "where
//! is `\foo` defined?" and "what does `\foo` do?", and go-to-definition,
//! find-references, rename, hover and the undefined-reference diagnostic all
//! read from it.
//!
//! # Known limitations
//!
//! - Only *top-level* assignments are definitions: the query is anchored on
//!   `lilypond_program`, so a `foo = …` nested inside a `\layout` or `\with`
//!   block is invisible. Scheme bindings are found at any depth, since they
//!   carry no such anchor.
//! - Nothing inside a music function's `#{ … #}` body is read, definitions
//!   included. That is what keeps the LilyPond and Scheme readers from becoming
//!   mutually recursive; see [`command::scheme`]'s module docs.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use streaming_iterator::StreamingIterator;
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, Position, Range, TextDocumentContentChangeEvent,
};
use tree_sitter::{InputEdit, Language, Node, Parser, Point, Query, QueryCursor, Tree};

use crate::command::definition::{self, Binding};
use crate::command::{self, Arg, Commands};
use crate::context;
use crate::line_struct::{LineIndex, Span};
use crate::note_analyser;
use crate::notes::{Events, NoteAnalysis, Problem};
use crate::vocabulary::{Layer, Scope};

/// A named occurrence in the source: either a definition (`foo = ...`) or a
/// reference (`\foo`). `span` covers the clickable extent of the occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub span: Span,
}

/// An `\include "path"` directive. `path` is the (possibly relative) string
/// given in the source; `span` covers the quoted string for navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Include {
    pub path: String,
    pub span: Span,
}

/// The outcome of analysing a parse tree.
struct Analysis {
    definitions: Vec<Symbol>,
    references: Vec<Symbol>,
    includes: Vec<Include>,
}

/// What a document with no file behind it calls itself, matching the scheme
/// LSP gives such a buffer's URI.
const UNTITLED: &str = "untitled";

/// An analysed document. Holds the source text and its parse tree (so edits
/// can be reparsed incrementally), a line index for position conversion, and
/// the symbols found within.
#[derive(Debug)]
pub struct Document {
    /// Shared rather than owned outright, so that every [`Variable`] this file
    /// binds can hold the text its value is written in without copying a byte
    /// of it. An edit builds a new one; nothing mutates it in place.
    ///
    /// [`Variable`]: crate::command::variable::Variable
    text: Arc<str>,
    /// What to call the file this document is, when hover says where a command
    /// it defines came from — see
    /// [`Layer::origin`](crate::vocabulary::Layer::origin). A name rather than
    /// a URI: a document knows nothing else of where it lives, and the graph
    /// that opened it is where a URI belongs. Kept here as well as in the layer
    /// so that an edit, which rebuilds the layer, keeps the name.
    origin: Arc<str>,
    tree: Tree,
    line_index: LineIndex,
    /// Every definition, in source order — the positional view of
    /// `commands_defined`, which is a map and so has no order of its own.
    /// Answers "what is under the cursor?"; the layer answers everything else.
    definitions: Vec<Symbol>,
    references: Vec<Symbol>,
    includes: Vec<Include>,
    /// What *this file* defines, ready to be stacked into the [`Scope`] of
    /// every document that can see it — commands, and the context types its
    /// `\context { … }` blocks declare. Read once here, when the document is
    /// built, which is what stops an include shared by a dozen scores being
    /// read a dozen times.
    commands_defined: Arc<Layer>,
    notes: NoteAnalysis,
    /// The scope `notes` was analysed in. Compared (by
    /// [`Scope::fingerprint`]) against the current scope by
    /// [`refresh`](Self::refresh), which is what makes an edit to an included
    /// file re-analyse the files that include it — and kept whole rather than
    /// as its fingerprint alone because it is also the answer to "what could
    /// be written here?", which completion asks. Cheap to hold: a [`Scope`] is
    /// a shared list of `Arc<Layer>`.
    analysed_in: Scope,
}

impl Document {
    /// A document with no file behind it, which is what LSP calls an
    /// `untitled:` buffer and what a test builds. It still defines commands,
    /// and hover still has to say where they came from, so it borrows the one
    /// word the editor would use for it.
    pub fn new(text: String) -> Self {
        Self::named(UNTITLED, text)
    }

    /// A document that came from somewhere: `origin` is the name to show for
    /// the commands it defines, and is the file's name rather than its whole
    /// path — hover wants the short answer, and the long one is a click away
    /// through go-to-definition.
    pub fn named(origin: impl Into<Arc<str>>, text: String) -> Self {
        let tree = parse(&text, None);
        Self::from_parts(Arc::from(text), origin.into(), tree)
    }

    /// Builds the derived state (line index, symbols, definitions and note
    /// analysis) for `text` and `tree`.
    ///
    /// The note analysis is done in the scope the file makes on its own — the
    /// builtins and its own definitions — since a document knows nothing of
    /// the graph it sits in; [`refresh`](Self::refresh) redoes it once the
    /// graph can say what else this document can see. A file that includes
    /// nothing (or nothing that defines a command) is therefore analysed here
    /// and never again.
    fn from_parts(text: Arc<str>, origin: Arc<str>, tree: Tree) -> Self {
        let line_index = LineIndex::new(&text);
        let analysis = extract(&tree, &text);
        let bindings = merge_bindings(&tree, &text, &analysis.definitions);
        let definitions = bindings
            .iter()
            .map(|binding| Symbol {
                name: binding.name.clone(),
                span: binding.span,
            })
            .collect();

        let context_types = context::read(&tree, &text)
            .into_iter()
            .map(|context_type| (context_type.name.clone(), context_type))
            .collect();
        let context_instances = context::read_instances(&tree, &text)
            .into_iter()
            .map(|instance| (instance.name.clone(), instance))
            .collect();

        let commands_defined = Arc::new(
            definition::layer(bindings, Arc::clone(&text), Arc::clone(&origin))
                .with_context_types(context_types)
                .with_context_instances(context_instances),
        );
        let scope = own_scope(&commands_defined);
        let notes = note_analyser::analyse(&tree, &text, &scope);
        Self {
            text,
            origin,
            tree,
            line_index,
            definitions,
            references: analysis.references,
            includes: analysis.includes,
            commands_defined,
            notes,
            analysed_in: scope,
        }
    }

    /// What this document can see: the layers it was last analysed in, which
    /// is what completion offers and what any other "what does `\foo` mean
    /// here?" question is answered from.
    pub(crate) fn scope(&self) -> &Scope {
        &self.analysed_in
    }

    /// The layer of commands this file defines, for stacking into a [`Scope`].
    pub(crate) fn commands_defined(&self) -> &Arc<Layer> {
        &self.commands_defined
    }

    /// Re-runs the note and command analysis if `scope` differs from the one
    /// it was last done in.
    ///
    /// Command parsing is a cross-file analysis: whether `\myFunc { c4 }` is a
    /// call with a music argument or a bare `\myFunc` followed by a block
    /// depends on a `define-music-function` that may live in an included file.
    /// So the analysis is keyed on the scope that produced it, and redone when
    /// a file in the include closure changes — which mints a new
    /// [`Layer`](crate::vocabulary::Layer) id, and so a new fingerprint. Doing
    /// it here, lazily, rather than eagerly invalidating dependants at the
    /// moment of an edit, means the cost falls only on documents actually
    /// queried, and needs no reverse include index to find them.
    pub(crate) fn refresh(&mut self, scope: &Scope) {
        if scope.fingerprint() == self.analysed_in.fingerprint() {
            return;
        }
        self.notes = note_analyser::analyse(&self.tree, &self.text, scope);
        self.analysed_in = scope.clone();
    }

    /// Applies a single LSP content change. A change with a `range` is spliced
    /// into the text and reparsed incrementally against the old tree; a change
    /// without one replaces the whole document.
    pub fn apply_change(&mut self, change: TextDocumentContentChangeEvent) {
        let Some(range) = change.range else {
            let tree = parse(&change.text, None);
            *self = Self::from_parts(Arc::from(change.text), Arc::clone(&self.origin), tree);
            return;
        };

        let start_byte = self.line_index.offset_clamped(range.start);
        let old_end_byte = self.line_index.offset_clamped(range.end);

        // tree-sitter points use byte columns; compute the start and old-end
        // points from the text as it stands before the splice.
        let start_position = point_at(&self.text, start_byte);
        let old_end_position = point_at(&self.text, old_end_byte);

        let text = splice(&self.text, start_byte, old_end_byte, &change.text);
        let new_end_byte = start_byte + change.text.len();
        let new_end_position = point_at(&text, new_end_byte);

        self.tree.edit(&InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position,
            old_end_position,
            new_end_position,
        });

        let tree = parse(&text, Some(&self.tree));
        *self = Self::from_parts(text, Arc::clone(&self.origin), tree);
    }

    /// The document's source text.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The line index, for converting between byte offsets and LSP positions.
    pub(crate) fn line_index(&self) -> &LineIndex {
        &self.line_index
    }

    /// The root of the parse tree.
    pub(crate) fn root_node(&self) -> Node<'_> {
        self.tree.root_node()
    }

    pub fn definitions(&self) -> &[Symbol] {
        &self.definitions
    }

    pub fn references(&self) -> &[Symbol] {
        &self.references
    }

    pub fn includes(&self) -> &[Include] {
        &self.includes
    }

    /// The lexically resolved music events, queryable by position or span.
    pub fn notes(&self) -> &Events {
        &self.notes.events
    }

    /// The structured command invocations (`\repeat`, `\volta`, …) found in the
    /// same pass, in source order.
    pub fn commands(&self) -> &Commands {
        &self.notes.commands
    }

    /// Syntax diagnostics from the parse tree. Tree-sitter's error recovery
    /// marks regions it couldn't parse as ERROR nodes and tokens it expected
    /// but didn't find as MISSING nodes; we surface both.
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let root = self.tree.root_node();
        if root.has_error() {
            self.collect_syntax_errors(root, &mut diagnostics);
        }
        for &problem in &self.notes.problems {
            let span = problem.span();
            let (severity, message) = match problem {
                Problem::NotANote(_) => (
                    DiagnosticSeverity::WARNING,
                    format!("`{}` is not a note name", &self.text[span.start..span.end]),
                ),
                Problem::DanglingColon(_) => (
                    DiagnosticSeverity::ERROR,
                    "expected a chord modifier or tremolo after `:`".to_string(),
                ),
            };
            diagnostics.push(Diagnostic {
                range: self.line_index.range_of(span),
                severity: Some(severity),
                source: Some("ly-lsp".to_string()),
                message,
                ..Diagnostic::default()
            });
        }
        diagnostics.extend(self.command_diagnostics());
        diagnostics
    }

    /// Runs [`Command::check`](command::Command::check) over every command
    /// call in the document. This is the whole-document pass
    /// [`CheckContext`](command::CheckContext)'s doc comment describes: a
    /// call's complaint can depend on other calls (`\volta` is only wrong
    /// because no `\repeat volta` encloses it), which aren't all collected
    /// until parsing of the whole document is done, so this runs here rather
    /// than inside the note analyser's walk.
    fn command_diagnostics(&self) -> Vec<Diagnostic> {
        let commands = self.commands();
        let ctx = command::CheckContext::new(&self.line_index, commands);
        commands
            .iter()
            .flat_map(|call| call.cmd.check(call, &ctx))
            .collect()
    }

    /// A diagnostic for every `\foo` reference whose name `is_known` rejects.
    ///
    /// The caller supplies the notion of "known" — typically a built-in command
    /// or a definition reachable through includes — so this stays oblivious to
    /// both the builtin list and the include graph.
    pub fn undefined_reference_diagnostics(
        &self,
        is_known: impl Fn(&str) -> bool,
    ) -> Vec<Diagnostic> {
        self.references
            .iter()
            .filter(|reference| !is_known(&reference.name))
            .map(|reference| Diagnostic {
                range: self.line_index.range_of(reference.span),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("ly-lsp".to_string()),
                message: format!("undefined reference to `\\{}`", reference.name),
                ..Diagnostic::default()
            })
            .collect()
    }

    /// Walks `node`, pushing a diagnostic for each ERROR and MISSING node and
    /// pruning subtrees that contain no errors.
    fn collect_syntax_errors(&self, node: Node, out: &mut Vec<Diagnostic>) {
        if node.is_missing() {
            out.push(self.syntax_error(node, format!("missing `{}`", node.kind())));
            return;
        }
        if node.is_error() {
            // Tree-sitter wraps a region it couldn't parse in a single ERROR
            // node, which on its own only tells us "something is wrong here".
            // An unbalanced bracket or quote is by far the most common cause,
            // so look for the offending delimiter and point at it directly;
            // fall back to the generic message when nothing stands out.
            let unbalanced = self.unbalanced_delimiters(node);
            if unbalanced.is_empty() {
                out.push(self.syntax_error(node, "syntax error".to_string()));
            } else {
                out.extend(unbalanced);
            }
            return;
        }
        if !node.has_error() {
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.collect_syntax_errors(child, out);
        }
    }

    /// Pairs off the bracket and quote tokens directly under an ERROR node and
    /// returns a diagnostic for each one left unmatched. Well-formed nested
    /// structures are folded into named children, so the loose delimiter tokens
    /// we see here are exactly the ones the parser couldn't balance.
    fn unbalanced_delimiters(&self, node: Node) -> Vec<Diagnostic> {
        let mut pending: Vec<Node> = Vec::new();
        let mut diagnostics = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "{" | "<<" | "<" => pending.push(child),
                "}" | ">>" | ">" => {
                    if pending
                        .last()
                        .is_some_and(|open| closes(open.kind(), child.kind()))
                    {
                        pending.pop();
                    } else {
                        diagnostics.push(
                            self.syntax_error(child, format!("unmatched `{}`", child.kind())),
                        );
                    }
                }
                // A quote is its own closer: it closes a string already open,
                // otherwise it opens one.
                "\"" => {
                    if pending.last().is_some_and(|open| open.kind() == "\"") {
                        pending.pop();
                    } else {
                        pending.push(child);
                    }
                }
                _ => {}
            }
        }
        for open in pending {
            diagnostics.push(self.syntax_error(open, format!("unclosed `{}`", open.kind())));
        }
        diagnostics
    }

    fn syntax_error(&self, node: Node, message: String) -> Diagnostic {
        let span = Span::new(node.start_byte(), node.end_byte());
        Diagnostic {
            range: self.line_index.range_of(span),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ly-lsp".to_string()),
            message,
            ..Diagnostic::default()
        }
    }

    /// Returns the include path under `position`, if the cursor is on one.
    pub fn include_at(&self, position: Position) -> Option<&str> {
        let offset = self.line_index.offset_at(position)?;
        self.includes
            .iter()
            .find(|i| i.span.contains(offset))
            .map(|i| i.path.as_str())
    }

    /// Returns the ranges of a matched bracket pair `[bracket, match]` if
    /// `position` is on a bracket token, or `None` if it is not.
    pub fn bracket_at(&self, position: Position) -> Option<[Range; 2]> {
        let offset = self.line_index.offset_at(position)?;
        let node = self
            .tree
            .root_node()
            .descendant_for_byte_range(offset, offset)?;

        let target_kind = match node.kind() {
            "{" => "}",
            "}" => "{",
            "<<" => ">>",
            ">>" => "<<",
            "<" => ">",
            ">" => "<",
            _ => return None,
        };
        let is_open = matches!(node.kind(), "{" | "<<" | "<");

        let node_span = Span::new(node.start_byte(), node.end_byte());
        let parent = node.parent()?;
        let mut cursor = parent.walk();
        let children: Vec<_> = parent.children(&mut cursor).collect();

        let match_node = if is_open {
            children
                .into_iter()
                .find(|c| c.kind() == target_kind && c.start_byte() > node.start_byte())
        } else {
            children
                .into_iter()
                .rfind(|c| c.kind() == target_kind && c.start_byte() < node.start_byte())
        }?;

        Some([
            self.line_index.range_of(node_span),
            self.line_index
                .range_of(Span::new(match_node.start_byte(), match_node.end_byte())),
        ])
    }

    /// Returns the name of the definition or reference under `position`, if any.
    pub fn symbol_at(&self, position: Position) -> Option<&str> {
        let offset = self.line_index.offset_at(position)?;
        self.definitions
            .iter()
            .chain(&self.references)
            .find(|s| s.span.contains(offset))
            .map(|s| s.name.as_str())
    }

    /// The `Arg::ContextType` or `Arg::ContextName` argument the cursor sits in
    /// at `position`, if there is one.
    ///
    /// Neither a context type (`Staff` in `\new Staff`) nor an instance name
    /// (`"vocals"` in `\lyricsto "vocals"`) is a `reference`
    /// [`Symbol`](Symbol) [`symbol_at`](Self::symbol_at) would find: both
    /// parse as a bare `symbol`/`string` node, meaningful only through the
    /// argument position `\new`'s (or `\lyricsto`'s, `\change`'s) signature
    /// gives it — go-to-definition and semantic tokens both need this same
    /// "is the cursor on one of these two argument kinds" lookup.
    ///
    /// Reuses [`Commands::call_site_at`], the same "which call, which
    /// argument position" lookup `command_assist::call_at` uses for
    /// signature help, completion and hover, rather than inventing a third
    /// way of finding the cursor inside a command's arguments. Its index can
    /// point one past the last argument actually parsed, for a call still
    /// being typed — the `span().contains` check is what tells that apart
    /// from the cursor really landing inside this one.
    fn context_arg_at(&self, position: Position) -> Option<&Arg> {
        let offset = self.line_index.offset_at(position)?;
        let site = self.commands().call_site_at(offset, self.text())?;
        let arg = site.call.args.get(site.index)?;
        (arg.span().contains(offset)
            && matches!(arg, Arg::ContextType { .. } | Arg::ContextName { .. }))
        .then_some(arg)
    }

    /// The name of the context type (`Staff` in `\new Staff`) under
    /// `position`, if the cursor is on one.
    pub(crate) fn context_type_at(&self, position: Position) -> Option<&str> {
        match self.context_arg_at(position)? {
            Arg::ContextType { name, .. } => Some(name.as_str()),
            _ => None,
        }
    }

    /// The name of the context instance (`"vocals"` in `\lyricsto "vocals"`,
    /// or the `"x"` a `\new`/`\context` names) under `position`, if the
    /// cursor is on one.
    pub(crate) fn context_name_at(&self, position: Position) -> Option<&str> {
        match self.context_arg_at(position)? {
            Arg::ContextName { name, .. } => Some(name.as_str()),
            _ => None,
        }
    }

    /// Definitions matching `name`, as LSP ranges, in source order.
    ///
    /// Asked of the layer rather than of [`definitions`](Self::definitions),
    /// since the layer is where a name's definitions are gathered: a file that
    /// binds `foo` twice has one entry, carrying both places through its
    /// [`redefines`](command::Command::redefines) chain.
    pub fn definition_ranges(&self, name: &str) -> Vec<Range> {
        self.commands_defined
            .get(name)
            .map(|command| {
                command::definition_spans(command.as_ref())
                    .into_iter()
                    .map(|span| self.line_index.range_of(span))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// References matching `name`, as LSP ranges.
    /// The single definition of `name` *in effect* at byte offset `at` — the
    /// last one written at or before it.
    ///
    /// A later definition doesn't erase an earlier one; it replaces it from the
    /// point it appears. LilyPond substitutes a variable where it is used, so
    /// in
    ///
    /// ```lilypond
    /// foo = { c }
    /// partOne = { \foo }
    /// foo = { d }
    /// partTwo = { \foo }
    /// ```
    ///
    /// the two `\foo`s are different music, and go-to-definition on each should
    /// land on the one it actually means rather than offering both.
    ///
    /// `at` is `None` when the reference is in another file. An `\include` is
    /// textually substituted, so every definition in *this* file precedes the
    /// reference and the last one is the one in effect.
    ///
    /// A reference preceding every definition of the name is LilyPond's error,
    /// not ours: the first definition is offered rather than nothing, since
    /// somewhere to look beats a dead end while the file is being written.
    pub fn definition_in_effect(&self, name: &str, at: Option<usize>) -> Option<Range> {
        let command = self.commands_defined.get(name)?;
        let spans = command::definition_spans(command.as_ref());
        let in_effect = match at {
            Some(at) => spans
                .iter()
                .take_while(|span| span.start <= at)
                .last()
                .or_else(|| spans.first()),
            None => spans.last(),
        };
        in_effect.map(|span| self.line_index.range_of(*span))
    }

    /// Where this file declares the context type `name` (`\context { \name
    /// Foo … }`'s `\name` span), if it declares one at all.
    ///
    /// Unlike [`definition_in_effect`](Self::definition_in_effect), there is
    /// no *in effect at a position* to resolve: a [`ContextType`] carries no
    /// [`redefines`](command::Command::redefines) chain the way a command
    /// binding does, so a file that writes `\name Foo` twice is read the
    /// same way [`ContextInstance`] already is for a repeated instance name
    /// (see its doc) — the layer keeps only the last one, and that is the
    /// one landed on however the cursor got here.
    ///
    /// Also `None` for a type declared only by the LilyPond install: this
    /// only ever looks at *this document's* own layer, which is the file it
    /// can point a [`Location`] at. An install-layer [`ContextType`] carries
    /// a `name_span` but no file
    /// (see `doc/command-parsing.md`, "Go-to-definition into the install"),
    /// so navigating to a built-in type like `Staff` degrades to no result
    /// here rather than a wrong one — the same known gap that document
    /// already records, not a new one.
    ///
    /// [`ContextType`]: crate::context::ContextType
    /// [`ContextInstance`]: crate::context::ContextInstance
    /// [`Location`]: tower_lsp::lsp_types::Location
    pub fn context_type_declaration(&self, name: &str) -> Option<Range> {
        self.commands_defined
            .get_context_type(name)
            .map(|context_type| self.line_index.range_of(context_type.name_span))
    }

    /// Where this file creates the context instance `name` (the `"vocals"`
    /// of a `\new Voice = "vocals"`/`\context Voice = "vocals"`), if it
    /// creates one at all.
    ///
    /// The [`ContextInstance`] counterpart of
    /// [`context_type_declaration`](Self::context_type_declaration) — same
    /// "only this file's own layer, no install" limitation, and no
    /// redefinition chain to resolve a cursor position against either.
    ///
    /// [`ContextInstance`]: crate::context::ContextInstance
    pub fn context_instance_creation(&self, name: &str) -> Option<Range> {
        self.commands_defined
            .get_context_instance(name)
            .map(|instance| self.line_index.range_of(instance.span))
    }

    pub fn reference_ranges(&self, name: &str) -> Vec<Range> {
        self.ranges(&self.references, name)
    }

    fn ranges(&self, symbols: &[Symbol], name: &str) -> Vec<Range> {
        symbols
            .iter()
            .filter(|s| s.name == name)
            .map(|s| self.line_index.range_of(s.span))
            .collect()
    }
}

/// The LilyPond grammar as a tree-sitter [`Language`].
fn language() -> Language {
    tree_sitter_lilypond::LANGUAGE_LILYPOND.into()
}

/// Query capturing the symbols we care about.
///
/// A `@definition` is the name `symbol` of a top-level assignment (a direct
/// child of `lilypond_program`), which excludes header fields, `\with` blocks
/// and `\override` property paths. A `@reference` is any `\foo` command; most
/// resolve to no definition because they're built-ins, which is exactly why
/// go-to-definition leaves them alone.
pub(crate) const SYMBOL_QUERY: &str = r#"
(lilypond_program (assignment_lhs (symbol) @definition))
(escaped_word) @reference
"#;

fn symbol_query() -> &'static Query {
    static QUERY: OnceLock<Query> = OnceLock::new();
    QUERY.get_or_init(|| Query::new(&language(), SYMBOL_QUERY).expect("valid query"))
}

/// Everything a file defines, from both readers, in source order.
///
/// `assignments` are what [`SYMBOL_QUERY`] captured; the Scheme reader supplies
/// the rest. The two overlap in exactly one shape: `myFunc = #(define-music-function …)`
/// is an assignment whose value the Scheme reader can read a signature out of,
/// and both name the *same* `symbol` node. Keeping the Scheme reader's binding
/// and dropping the query's — matched on the span, since that node is what
/// makes them the same definition — is what stops go-to-definition offering the
/// same place twice while still giving the name its signature.
fn merge_bindings(tree: &Tree, src: &str, assignments: &[Symbol]) -> Vec<Binding> {
    let mut bindings = command::scheme::read(tree, src);
    let bound: HashSet<usize> = bindings.iter().map(|binding| binding.span.start).collect();
    let values = assignment_values(tree, src);
    bindings.extend(
        assignments
            .iter()
            .filter(|symbol| !bound.contains(&symbol.span.start))
            .map(|symbol| {
                Binding::variable(
                    symbol.name.clone(),
                    symbol.span,
                    values.get(&symbol.span.start).copied(),
                )
            }),
    );
    bindings.sort_by_key(|binding| binding.span.start);
    bindings
}

/// Where each top-level assignment's value is written, keyed by the start of
/// the name it binds — the offset the [`SYMBOL_QUERY`] capture and the
/// `assignment_lhs` share, being the same place in the file.
fn assignment_values(tree: &Tree, src: &str) -> HashMap<usize, Span> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let children: Vec<Node> = root.children(&mut cursor).collect();

    let mut values = HashMap::new();
    let mut i = 0;
    while i < children.len() {
        if children[i].kind() != "assignment_lhs" {
            i += 1;
            continue;
        }
        let value = assignment_value(&children, i);
        if let Some(span) = value_span(&children[value.clone()], src) {
            values.insert(children[i].start_byte(), span);
        }
        i = value.end.max(i + 1);
    }
    values
}

/// The run of siblings making up the value of the assignment whose left-hand
/// side is `children[lhs]`, as a range into `children`.
///
/// The grammar is flat: `foo = \relative c' { … }` is not one node but a row of
/// them, with nothing marking where the value ends. So bound it at the first
/// complete music expression — consume up to and including the first block —
/// stopping early at the next top-level assignment, which a value can never
/// reach past. A value with no block at all (`foo = \bar`, `foo = #5`) runs to
/// that next assignment, or to the end of the file.
pub(crate) fn assignment_value(children: &[Node], lhs: usize) -> std::ops::Range<usize> {
    let mut i = lhs + 1;
    if children.get(i).map(Node::kind) == Some("punctuation") {
        i += 1;
    }
    let start = i;
    while i < children.len() && children[i].kind() != "assignment_lhs" {
        let kind = children[i].kind();
        i += 1;
        if kind == "expression_block" || kind == "parallel_music" {
            break;
        }
    }
    start..i
}

/// The trimmed extent of a run of nodes; `None` for an empty run, which is what
/// an assignment with nothing after its `=` leaves.
pub(crate) fn value_span(nodes: &[Node], src: &str) -> Option<Span> {
    let start = nodes.first()?.start_byte();
    let end = nodes.last()?.end_byte();
    // Node boundaries exclude surrounding space already; trim defensively.
    let raw = &src[start..end];
    let lead = raw.len() - raw.trim_start().len();
    let trail = raw.len() - raw.trim_end().len();
    Some(Span::new(start + lead, end - trail))
}

/// Parses `src` fresh and reads everything it binds, from both readers, in
/// source order — the same merge [`from_parts`](Document::from_parts) folds
/// into a `Document`'s layer, but usable without building a whole `Document`
/// (and without running note analysis over source that will never be
/// queried for notes).
///
/// [`crate::install`] is the reason this exists: LilyPond's own
/// initialisation files are ordinary `.ly` files as far as either reader is
/// concerned, so reading them for the commands they bind needs nothing a
/// `Document` does beyond this.
pub(crate) fn parse_bindings(src: &str) -> Vec<Binding> {
    let tree = parse(src, None);
    let analysis = extract(&tree, src);
    merge_bindings(&tree, src, &analysis.definitions)
}

/// The scope a file makes on its own: the hand-written layers, plus whatever
/// it defines itself. Assembled by the same [`Scope::for_document`] the graph
/// uses, so a document with no includes fingerprints identically either way
/// and is never re-analysed for the sake of it.
fn own_scope(defined: &Arc<Layer>) -> Scope {
    Scope::builtins().for_document(std::slice::from_ref(defined))
}

/// Parses `src`, reusing `old_tree` for incremental reparsing when supplied.
pub(crate) fn parse(src: &str, old_tree: Option<&Tree>) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&language())
        .expect("load LilyPond grammar");
    parser
        .parse(src, old_tree)
        .expect("parser produces a tree for any input")
}

/// Extracts definitions, references and includes from `tree`.
///
/// `\include` is captured as an `escaped_word` like any other command, then
/// recognised here by its text and paired with the string literal that follows
/// it, so it becomes an [`Include`] rather than a (meaningless) reference.
fn extract(tree: &Tree, src: &str) -> Analysis {
    let query = symbol_query();
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut definitions = Vec::new();
    let mut references = Vec::new();
    let mut includes = Vec::new();

    let mut matches = cursor.matches(query, tree.root_node(), src.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let node = cap.node;
            let span = Span::new(node.start_byte(), node.end_byte());
            let text = &src[span.start..span.end];
            match capture_names[cap.index as usize] {
                "definition" => definitions.push(Symbol {
                    name: text.to_string(),
                    span,
                }),
                "reference" if text == "\\include" => {
                    includes.extend(include_after(node, src));
                }
                "reference" => references.push(Symbol {
                    // Strip the leading backslash from `\foo`.
                    name: text[1..].to_string(),
                    span,
                }),
                _ => {}
            }
        }
    }

    Analysis {
        definitions,
        references,
        includes,
    }
}

/// Reads the `\include` target from the string literal following the `\include`
/// keyword node, if present.
fn include_after(keyword: Node, src: &str) -> Option<Include> {
    let string_node = keyword.next_named_sibling()?;
    if string_node.kind() != "string" {
        return None;
    }
    let mut cursor = string_node.walk();
    let fragment = string_node
        .named_children(&mut cursor)
        .find(|n| n.kind() == "string_fragment")?;
    Some(Include {
        path: src[fragment.start_byte()..fragment.end_byte()].to_string(),
        // The span covers the whole quoted string so clicking the path works.
        span: Span::new(string_node.start_byte(), string_node.end_byte()),
    })
}

/// Computes the tree-sitter [`Point`] (row, byte-column) for a byte offset into
/// `text`. Unlike LSP positions, tree-sitter columns are counted in bytes.
/// `text` with `start..end` replaced by `replacement`, as a fresh shared
/// string. Builds a new allocation rather than editing in place because the old
/// one is shared with the [`Layer`] built from it — and an in-place
/// `replace_range` would move the same bytes anyway.
fn splice(text: &str, start: usize, end: usize, replacement: &str) -> Arc<str> {
    let mut spliced = String::with_capacity(text.len() - (end - start) + replacement.len());
    spliced.push_str(&text[..start]);
    spliced.push_str(replacement);
    spliced.push_str(&text[end..]);
    Arc::from(spliced)
}

fn point_at(text: &str, byte: usize) -> Point {
    let before = &text[..byte];
    let row = before.bytes().filter(|&b| b == b'\n').count();
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Point::new(row, byte - line_start)
}

/// Whether `close` is the closing delimiter matching the opener `open`.
fn closes(open: &str, close: &str) -> bool {
    matches!((open, close), ("{", "}") | ("<<", ">>") | ("<", ">"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(symbols: &[Symbol]) -> Vec<&str> {
        symbols.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn finds_top_level_definition() {
        let doc = Document::new("foo = { c d e }".to_string());
        assert_eq!(names(doc.definitions()), vec!["foo"]);
    }

    #[test]
    fn finds_reference() {
        let doc = Document::new("\\foo".to_string());
        assert_eq!(names(doc.references()), vec!["foo"]);
    }

    #[test]
    fn header_field_is_not_a_definition() {
        // `title` is a header field nested in a block, not a top-level variable.
        let doc = Document::new("\\header { title = \"X\" }".to_string());
        assert!(doc.definitions().is_empty());
    }

    #[test]
    fn override_property_is_not_a_definition() {
        let doc = Document::new("\\score { \\override NoteHead.color = #red }".to_string());
        assert!(doc.definitions().is_empty());
    }

    #[test]
    fn builtins_are_references_without_definitions() {
        let doc = Document::new("\\relative c' { c d e }".to_string());
        assert_eq!(names(doc.references()), vec!["relative"]);
        assert!(doc.definition_ranges("relative").is_empty());
    }

    #[test]
    fn comment_contents_are_ignored() {
        // Neither the assignment nor the command inside a comment count.
        let doc = Document::new("% foo = \\bar\nbaz = { c }\n".to_string());
        assert_eq!(names(doc.definitions()), vec!["baz"]);
        assert!(doc.references().is_empty());
    }

    #[test]
    fn string_contents_are_ignored() {
        let doc = Document::new("title = \"a \\foo b\"".to_string());
        assert!(doc.references().is_empty());
    }

    #[test]
    fn symbol_at_definition_and_reference() {
        let src = "foo = { c }\n\\foo\n";
        let doc = Document::new(src.to_string());
        // On the `foo` of the definition (line 0, char 1).
        assert_eq!(doc.symbol_at(Position::new(0, 1)), Some("foo"));
        // On the `\foo` reference (line 1, char 2, i.e. the first `o`).
        assert_eq!(doc.symbol_at(Position::new(1, 2)), Some("foo"));
        // On whitespace.
        assert_eq!(doc.symbol_at(Position::new(0, 4)), None);
    }

    #[test]
    fn definition_and_reference_ranges() {
        let src = "foo = { c }\n\\foo\n";
        let doc = Document::new(src.to_string());
        let defs = doc.definition_ranges("foo");
        assert_eq!(
            defs,
            vec![Range::new(Position::new(0, 0), Position::new(0, 3))]
        );
        let refs = doc.reference_ranges("foo");
        assert_eq!(
            refs,
            vec![Range::new(Position::new(1, 0), Position::new(1, 4))]
        );
    }

    #[test]
    fn utf16_positions_account_for_wide_characters() {
        // `café` then a definition; the é is one UTF-16 unit but two bytes.
        // A musical 𝄞 (U+1D11E) is two UTF-16 units and four bytes.
        let src = "% café 𝄞\nfoo = 1\n";
        let doc = Document::new(src.to_string());
        let defs = doc.definition_ranges("foo");
        assert_eq!(
            defs,
            vec![Range::new(Position::new(1, 0), Position::new(1, 3))]
        );

        // Round-trip a position on the second line.
        let index = LineIndex::new(src);
        let pos = Position::new(1, 0);
        let offset = index.offset_at(pos).unwrap();
        assert_eq!(index.position_at(offset), pos);
    }

    #[test]
    fn multiple_references_to_same_variable() {
        let src = "foo = { c }\n\\foo \\foo\n";
        let doc = Document::new(src.to_string());
        assert_eq!(doc.reference_ranges("foo").len(), 2);
    }

    /// Builds a ranged content change replacing `range` with `text`.
    fn change(range: Range, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(range),
            range_length: None,
            text: text.to_string(),
        }
    }

    #[test]
    fn incremental_edit_renames_definition() {
        let mut doc = Document::new("foo = { c }\n\\foo\n".to_string());
        // Replace the `foo` on line 0 (chars 0..3) with `bar`.
        doc.apply_change(change(
            Range::new(Position::new(0, 0), Position::new(0, 3)),
            "bar",
        ));

        assert_eq!(names(doc.definitions()), vec!["bar"]);
        assert_eq!(doc.definition_ranges("bar").len(), 1);
        assert!(doc.definition_ranges("foo").is_empty());
        // The reference is unchanged and now dangles.
        assert_eq!(names(doc.references()), vec!["foo"]);
    }

    #[test]
    fn incremental_insert_shifts_later_positions() {
        let mut doc = Document::new("foo = { c }\n\\foo\n".to_string());
        // Insert a new top-level definition and a blank line at the very start.
        doc.apply_change(change(
            Range::new(Position::new(0, 0), Position::new(0, 0)),
            "bar = { d }\n",
        ));

        assert_eq!(names(doc.definitions()), vec!["bar", "foo"]);
        // `foo`'s definition has moved from line 0 to line 1.
        assert_eq!(
            doc.definition_ranges("foo"),
            vec![Range::new(Position::new(1, 0), Position::new(1, 3))]
        );
        // The `\foo` reference has shifted down to line 2.
        assert_eq!(
            doc.reference_ranges("foo"),
            vec![Range::new(Position::new(2, 0), Position::new(2, 4))]
        );
    }

    #[test]
    fn full_replacement_change_replaces_document() {
        let mut doc = Document::new("foo = { c }\n".to_string());
        // A change with no range carries the entire new document.
        doc.apply_change(TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "baz = { e }\n\\baz\n".to_string(),
        });

        assert_eq!(names(doc.definitions()), vec!["baz"]);
        assert_eq!(names(doc.references()), vec!["baz"]);
        assert!(doc.definition_ranges("foo").is_empty());
    }

    #[test]
    fn well_formed_source_has_no_diagnostics() {
        let doc = Document::new("foo = { c d e }\n\\foo\n".to_string());
        assert!(doc.diagnostics().is_empty());
    }

    #[test]
    fn repeat_with_a_valid_kind_is_not_flagged() {
        let doc = Document::new("\\repeat volta 2 { c }".to_string());
        assert!(doc.diagnostics().is_empty());
    }

    #[test]
    fn repeat_with_an_unrecognised_kind_is_flagged() {
        let doc = Document::new("\\repeat bogus 2 { c }".to_string());
        let diagnostics = doc.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostics[0].message.contains("bogus"));
        // Localised to the offending word itself, not the whole call.
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(0, 8), Position::new(0, 13))
        );
    }

    #[test]
    fn repeat_with_a_count_of_one_is_not_flagged() {
        let doc = Document::new("\\repeat unfold 1 { c }".to_string());
        assert!(doc.diagnostics().is_empty());
    }

    #[test]
    fn repeat_with_a_count_of_zero_is_flagged() {
        let doc = Document::new("\\repeat unfold 0 { c }".to_string());
        let diagnostics = doc.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostics[0].message.contains('0'));
    }

    #[test]
    fn volta_is_never_flagged_regardless_of_enclosing_context() {
        // `\volta` is valid inside any `\repeat` kind, in an `\alternative`
        // that follows (rather than sits inside) the repeat, and even in a
        // variable substituted into a `\repeat` body from elsewhere in the
        // file — none of which lexical enclosure can see, so `\volta` gets no
        // enclosing-repeat check at all.
        for src in [
            "\\repeat volta 2 { \\volta 1 { c } }",
            "\\repeat segno 2 { \\volta 1 { c } }",
            "\\repeat unfold 2 { \\volta 1 { c } }",
            "{ \\volta 1 { c } }",
        ] {
            let doc = Document::new(src.to_string());
            assert!(doc.diagnostics().is_empty(), "flagged: {src}");
        }
    }

    #[test]
    fn unclosed_brace_is_reported_at_the_brace() {
        let doc = Document::new("foo = { c d e\n".to_string());
        let diagnostics = doc.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostics[0].message, "unclosed `{`");
        // Localised to the `{` itself, not the whole region after it.
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(0, 6), Position::new(0, 7))
        );
    }

    #[test]
    fn unclosed_quote_is_reported_at_the_quote() {
        let doc = Document::new("foo = { c \"hello\n".to_string());
        let messages: Vec<String> = doc.diagnostics().into_iter().map(|d| d.message).collect();
        assert!(
            messages.iter().any(|m| m == "unclosed `\"`"),
            "expected an unclosed-quote diagnostic, got {messages:?}"
        );
    }

    #[test]
    fn unmatched_close_brace_is_reported_at_the_brace() {
        let doc = Document::new("foo = { c d } }\n".to_string());
        let diagnostics = doc.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "unmatched `}`");
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(0, 14), Position::new(0, 15))
        );
    }

    #[test]
    fn parses_include_directives() {
        let doc =
            Document::new("\\include \"notes.ily\"\n\\include \"parts/violin.ily\"\n".to_string());
        let paths: Vec<&str> = doc.includes().iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, vec!["notes.ily", "parts/violin.ily"]);
        // `\include` itself is not a reference.
        assert!(doc.references().is_empty());
    }

    #[test]
    fn include_at_finds_path_under_cursor() {
        let doc = Document::new("\\include \"notes.ily\"\n".to_string());
        // Cursor inside the quoted path (char 12 is within "notes.ily").
        assert_eq!(doc.include_at(Position::new(0, 12)), Some("notes.ily"));
        // Cursor on the `\include` keyword resolves to no include.
        assert_eq!(doc.include_at(Position::new(0, 2)), None);
    }

    #[test]
    fn bracket_at_sequential_music() {
        let doc = Document::new("foo = { c d e }\n".to_string());
        // Cursor on `{` (char 6).
        let [open, close] = doc.bracket_at(Position::new(0, 6)).expect("bracket pair");
        assert_eq!(open, Range::new(Position::new(0, 6), Position::new(0, 7)));
        assert_eq!(
            close,
            Range::new(Position::new(0, 14), Position::new(0, 15))
        );
        // Cursor on `}` (char 14) — same pair, reversed.
        let [close2, open2] = doc.bracket_at(Position::new(0, 14)).expect("bracket pair");
        assert_eq!(
            close2,
            Range::new(Position::new(0, 14), Position::new(0, 15))
        );
        assert_eq!(open2, Range::new(Position::new(0, 6), Position::new(0, 7)));
    }

    #[test]
    fn bracket_at_simultaneous_music() {
        let doc = Document::new("<< { c } { d } >>\n".to_string());
        // Cursor on first `<` of `<<` (char 0).
        let [open, close] = doc.bracket_at(Position::new(0, 0)).expect("bracket pair");
        assert_eq!(open, Range::new(Position::new(0, 0), Position::new(0, 2)));
        assert_eq!(
            close,
            Range::new(Position::new(0, 15), Position::new(0, 17))
        );
    }

    #[test]
    fn bracket_at_chord() {
        let doc = Document::new("{ <c e g> }\n".to_string());
        // Cursor on `<` (char 2).
        let [open, close] = doc
            .bracket_at(Position::new(0, 2))
            .expect("chord bracket pair");
        assert_eq!(open, Range::new(Position::new(0, 2), Position::new(0, 3)));
        assert_eq!(close, Range::new(Position::new(0, 8), Position::new(0, 9)));
    }

    #[test]
    fn bracket_at_note_returns_none() {
        let doc = Document::new("{ c d e }\n".to_string());
        // Cursor on `c` (a note, char 2) — not a bracket.
        assert!(doc.bracket_at(Position::new(0, 2)).is_none());
    }

    /// Diagnostics for references that resolve to none of `doc`'s own
    /// definitions — a single-document stand-in for [`DocumentGraph`]'s
    /// include-aware `is_known`, good enough for these tests since they don't
    /// span files.
    ///
    /// [`DocumentGraph`]: crate::document_graph::DocumentGraph
    fn undefined_references(doc: &Document) -> Vec<Diagnostic> {
        doc.undefined_reference_diagnostics(|name| doc.definitions().iter().any(|d| d.name == name))
    }

    #[test]
    fn music_function_definition_resolves_its_reference() {
        // `myFunc = #(define-music-function …)` is an ordinary assignment as far
        // as the symbol query is concerned — go-to-definition, find-references
        // and the undefined-reference diagnostic all see it exactly like
        // `myFunc = { c d e }`. Pinned here so that [`command::scheme`], which
        // reads *inside* the `#( … )` for the same definition, can't quietly
        // regress this simpler view of it.
        let doc = Document::new(
            "myFunc = #(define-music-function (m) (ly:music?) m)\n\\myFunc { c4 }\n".to_string(),
        );
        assert_eq!(names(doc.definitions()), vec!["myFunc"]);
        assert_eq!(names(doc.references()), vec!["myFunc"]);
        assert!(undefined_references(&doc).is_empty());
    }

    #[test]
    fn reference_with_no_definition_anywhere_is_undefined() {
        let doc = Document::new("\\myFunc { c4 }\n".to_string());
        assert!(doc.definitions().is_empty());
        let diagnostics = undefined_references(&doc);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("myFunc"));
    }

    #[test]
    fn a_scheme_only_definition_is_both_a_symbol_and_a_command() {
        // A function defined purely inside a Scheme block — no `foo = …` on the
        // left — produces no `assignment_lhs`, so [`SYMBOL_QUERY`] can't see it.
        // [`command::scheme`] reads the binding instead and reports the name it
        // binds, with the span of the name as written, so go-to-definition,
        // find-references, rename and the undefined-reference diagnostic all
        // treat it exactly like `myFunc = …`.
        let src =
            "#(define-public myFunc (define-music-function (m) (ly:music?) m))\n\\myFunc { c4 }\n";
        let doc = Document::new(src.to_string());

        assert_eq!(names(doc.definitions()), vec!["myFunc"]);
        let span = doc.definitions()[0].span;
        assert_eq!(&src[span.start..span.end], "myFunc");
        assert_eq!(names(doc.references()), vec!["myFunc"]);
        assert!(undefined_references(&doc).is_empty());
        assert!(
            doc.commands_defined().get("myFunc").is_some(),
            "and it carries a signature, being a definition form"
        );
    }

    #[test]
    fn a_scheme_binding_of_a_plain_value_is_a_definition_with_no_signature() {
        // `\foo` reaches anything bound in the module, so the binding is a
        // definition whatever its value — but only the `define-…-function`
        // forms say what arguments `\foo` takes, so this one takes none.
        let doc = Document::new("#(define-public myColour \"red\")\n\\myColour\n".to_string());
        assert_eq!(names(doc.definitions()), vec!["myColour"]);
        assert!(undefined_references(&doc).is_empty());
        assert!(
            doc.commands_defined()
                .get("myColour")
                .expect("a definition, callable as \\myColour")
                .signature()
                .is_empty()
        );
    }

    #[test]
    fn a_scheme_procedure_is_not_a_definition() {
        // `(define (helper x) …)` names a procedure, not an identifier `\helper`
        // could reach: its name is a list rather than a symbol, which is how
        // the two are told apart.
        let doc = Document::new("#(define (helper x) (* x 2))\n".to_string());
        assert!(doc.definitions().is_empty());
    }

    #[test]
    fn a_name_assigned_twice_keeps_both_definitions() {
        // LilyPond takes the later binding, so that is what `\foo` resolves to
        // — but both places are still written, and go-to-definition offers
        // both, as it did when definitions were a flat list.
        let doc = Document::new("foo = { c }\nbar = { d }\nfoo = { e }\n".to_string());
        assert_eq!(names(doc.definitions()), vec!["foo", "bar", "foo"]);
        assert_eq!(
            doc.definition_ranges("foo"),
            vec![
                Range::new(Position::new(0, 0), Position::new(0, 3)),
                Range::new(Position::new(2, 0), Position::new(2, 3)),
            ]
        );
    }

    #[test]
    fn a_reference_resolves_to_the_definition_in_effect_where_it_is_written() {
        // The two `\foo`s are different music: the first means `{ c }`, the
        // second `{ d }`. Each should land on the one it actually means.
        let src = "foo = { c }\npartOne = { \\foo }\nfoo = { d }\npartTwo = { \\foo }\n";
        let doc = Document::new(src.to_string());
        let first = src.find("\\foo").expect("the first reference");
        let second = src.rfind("\\foo").expect("the second reference");

        assert_eq!(
            doc.definition_in_effect("foo", Some(first)),
            Some(Range::new(Position::new(0, 0), Position::new(0, 3)))
        );
        assert_eq!(
            doc.definition_in_effect("foo", Some(second)),
            Some(Range::new(Position::new(2, 0), Position::new(2, 3)))
        );
    }

    #[test]
    fn without_a_position_the_last_definition_is_the_one_in_effect() {
        // What a reference in an *including* file sees: the whole of this file
        // precedes it, so the last binding wins.
        let doc = Document::new("foo = { c }\nfoo = { d }\n".to_string());
        assert_eq!(
            doc.definition_in_effect("foo", None),
            Some(Range::new(Position::new(1, 0), Position::new(1, 3)))
        );
    }

    #[test]
    fn a_reference_before_every_definition_falls_back_to_the_first() {
        // LilyPond wouldn't resolve this at all, but a file mid-edit is full of
        // such moments; somewhere to look beats a dead end.
        let src = "partOne = { \\foo }\nfoo = { c }\n";
        let doc = Document::new(src.to_string());
        assert_eq!(
            doc.definition_in_effect("foo", Some(src.find("\\foo").unwrap())),
            Some(Range::new(Position::new(1, 0), Position::new(1, 3)))
        );
    }

    #[test]
    fn a_cursor_on_a_definition_resolves_to_that_definition() {
        // Not to whichever one happens to be last: the cursor is *at* the
        // definition, so that is the one in effect there.
        let src = "foo = { c }\nfoo = { d }\n";
        let doc = Document::new(src.to_string());
        assert_eq!(
            doc.definition_in_effect("foo", Some(0)),
            Some(Range::new(Position::new(0, 0), Position::new(0, 3)))
        );
    }

    #[test]
    fn a_later_definition_wins_and_carries_the_one_it_replaced() {
        // The signature is the later definition's; the earlier one is still
        // reachable behind it, which is what a "this shadows an earlier
        // definition" warning would read.
        let src = "myFunc = { c }\n\
                   myFunc = #(define-music-function (m) (ly:music?) m)\n";
        let doc = Document::new(src.to_string());
        let my_func = doc.commands_defined().get("myFunc").expect("myFunc");
        assert_eq!(my_func.signature().len(), 1);
        let replaced = my_func.redefines().expect("the plain variable it replaced");
        assert!(replaced.signature().is_empty());
    }

    #[test]
    fn a_document_exposes_a_context_type_its_own_layout_block_declares() {
        // Step 2 wires `context::read` into `from_parts`, so a file's own
        // `\context { \name … }` declaration lands in the same layer as its
        // commands, not just in a reader nobody calls yet.
        let src = "\\score { { c } \\layout { \\context { \\name MyStaff } } }";
        let doc = Document::new(src.to_string());
        let declared = doc
            .commands_defined()
            .get_context_type("MyStaff")
            .expect("MyStaff");
        assert_eq!(declared.name, "MyStaff");
    }

    #[test]
    fn a_document_exposes_a_context_instance_its_own_music_creates() {
        // Step 5's counterpart of the context-type test above: `context::
        // read_instances` is wired into `from_parts` the same way.
        let src = "{ \\new Voice = \"vocals\" { c } }";
        let doc = Document::new(src.to_string());
        let created = doc
            .commands_defined()
            .get_context_instance("vocals")
            .expect("vocals");
        assert_eq!(created.name, "vocals");
        assert_eq!(created.type_name.as_deref(), Some("Voice"));
    }

    #[test]
    fn rebinding_a_context_instance_name_keeps_the_last() {
        // LilyPond takes the last binding of a name wherever it can be
        // rebound at all; a file that creates two same-named instances (a
        // score reusing a common voice name across movements, say) is read
        // the same way `commands_defined` already does for commands.
        let src = "{ \\new Staff = \"x\" { c } \\new Voice = \"x\" { c } }";
        let doc = Document::new(src.to_string());
        let bound = doc.commands_defined().get_context_instance("x").expect("x");
        assert_eq!(bound.type_name.as_deref(), Some("Voice"));
    }

    #[test]
    fn context_type_at_finds_the_name_in_a_new() {
        let src = "{ \\new Staff { c } }";
        let doc = Document::new(src.to_string());
        // Cursor inside `Staff` (the `t`).
        let pos = doc.line_index().position_at(src.find("Staff").unwrap() + 1);
        assert_eq!(doc.context_type_at(pos), Some("Staff"));
        assert_eq!(doc.context_name_at(pos), None);
    }

    #[test]
    fn context_name_at_finds_a_quoted_instance_name() {
        let src = "{ \\lyricsto \"vocals\" { la } }";
        let doc = Document::new(src.to_string());
        // Cursor inside `vocals`.
        let pos = doc
            .line_index()
            .position_at(src.find("vocals").unwrap() + 1);
        assert_eq!(doc.context_name_at(pos), Some("vocals"));
        assert_eq!(doc.context_type_at(pos), None);
    }

    #[test]
    fn context_type_and_name_at_are_none_elsewhere() {
        let src = "{ \\new Staff = \"x\" { c } }";
        let doc = Document::new(src.to_string());
        // On a note, well away from either argument.
        let pos = doc.line_index().position_at(src.find('c').unwrap());
        assert_eq!(doc.context_type_at(pos), None);
        assert_eq!(doc.context_name_at(pos), None);
    }

    #[test]
    fn context_type_declaration_locates_the_name_span() {
        let src = "\\layout { \\context { \\name MyStaff } }";
        let doc = Document::new(src.to_string());
        let range = doc
            .context_type_declaration("MyStaff")
            .expect("MyStaff is declared");
        let start = doc.line_index().offset_at(range.start).unwrap();
        let end = doc.line_index().offset_at(range.end).unwrap();
        assert_eq!(&src[start..end], "MyStaff");
    }

    #[test]
    fn context_type_declaration_is_none_for_an_undeclared_name() {
        // `Staff` here is only ever referred to, never declared with `\name`
        // — the shape a built-in type looks like from a file that never
        // declares its own context types at all.
        let doc = Document::new("{ \\new Staff { c } }".to_string());
        assert!(doc.context_type_declaration("Staff").is_none());
    }

    #[test]
    fn context_instance_creation_locates_the_name_span() {
        let src = "{ \\new Voice = \"vocals\" { c } }";
        let doc = Document::new(src.to_string());
        let range = doc
            .context_instance_creation("vocals")
            .expect("vocals is created");
        let start = doc.line_index().offset_at(range.start).unwrap();
        let end = doc.line_index().offset_at(range.end).unwrap();
        assert_eq!(&src[start..end], "vocals");
    }

    #[test]
    fn context_instance_creation_is_none_for_an_uncreated_name() {
        let doc = Document::new("{ \\lyricsto \"vocals\" { la } }".to_string());
        assert!(doc.context_instance_creation("vocals").is_none());
    }

    #[test]
    fn a_variable_reference_consumes_nothing_after_it() {
        // `\foo` now resolves to a command, but a zero-argument one: the block
        // after it is the enclosing music's, not an argument, and its notes are
        // read as notes.
        let doc = Document::new("foo = { c }\n{ \\foo { d e } }\n".to_string());
        assert!(doc.diagnostics().is_empty());
        let call = doc
            .commands()
            .iter()
            .find(|call| call.name == "foo")
            .expect("a call to the variable");
        assert!(call.args.is_empty());
        assert_eq!(doc.notes().iter().count(), 3, "c, d and e are all notes");
    }

    #[test]
    fn a_documents_own_music_function_is_in_scope_for_its_own_calls() {
        // No graph, no includes: a file's own definitions are readable from the
        // file alone, so its calls are parsed against them from the moment it
        // is parsed.
        let doc = Document::new(
            "myFunc = #(define-music-function (from music) (ly:pitch? ly:music?) music)\n\
             \\myFunc c' { c4 }\n"
                .to_string(),
        );
        let call = doc
            .commands()
            .iter()
            .find(|call| call.name == "myFunc")
            .expect("the call is parsed against its own definition");
        assert!(matches!(call.args[0], crate::command::Arg::Pitch { .. }));
        assert!(matches!(call.args[1], crate::command::Arg::Music { .. }));
    }

    #[test]
    fn edit_into_a_comment_drops_the_definition() {
        // Commenting out a definition by prefixing the line with `% ` should
        // make it disappear after reparsing.
        let mut doc = Document::new("foo = { c }\n".to_string());
        doc.apply_change(change(
            Range::new(Position::new(0, 0), Position::new(0, 0)),
            "% ",
        ));
        assert!(doc.definitions().is_empty());
    }

    mod proptests {
        use proptest::prelude::*;

        use super::*;

        /// A small LilyPond-ish alphabet: note letters (which also double as
        /// the stuff of multi-letter symbol names, so an assignment or a
        /// reference can appear by chance), digits, the punctuation an
        /// assignment or call needs (`{ } = \ "`), and whitespace including
        /// newlines. Deliberately not constrained to well-formed LilyPond —
        /// `Document::new` recovers from garbage via tree-sitter's error
        /// nodes, and the property under test holds regardless of
        /// well-formedness, so garbage is as good a source of cases as
        /// anything hand-picked.
        fn text_strategy() -> impl Strategy<Value = String> {
            "[a-g0-9=\"{}\\\\ \n]{0,40}"
        }

        /// A prospective edit, as fractions of the *current* document rather
        /// than fixed offsets: the actual range is only known once applied,
        /// since it depends on the text as it stands after the previous edit
        /// in the sequence. `start_fraction` and `end_fraction` are mapped
        /// onto char boundaries of the live text and ordered, so the range is
        /// always valid regardless of which is larger.
        fn edit_strategy() -> impl Strategy<Value = (f64, f64, String)> {
            (0.0f64..=1.0, 0.0f64..=1.0, "[a-g0-9=\"{}\\\\ \n]{0,5}")
        }

        /// The char boundary `fraction` of the way through `boundary_count`
        /// available ones (including the one past the end of the text).
        fn boundary_index(fraction: f64, boundary_count: usize) -> usize {
            let last = boundary_count - 1;
            ((fraction * last as f64).round() as usize).min(last)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// Applying any sequence of ranged edits incrementally must leave
            /// the document indistinguishable from constructing a fresh one
            /// from the resulting text: same text, same definitions and
            /// references (name and span alike), same diagnostics. Incremental
            /// reparsing is an optimisation over `Document::new`, not a
            /// different code path with its own semantics, and the three
            /// hand-written tests above each pin one instance of that; this
            /// is the general claim.
            #[test]
            fn incremental_editing_matches_a_full_reparse(
                initial in text_strategy(),
                edits in prop::collection::vec(edit_strategy(), 1..5),
            ) {
                let mut doc = Document::new(initial);
                for (start_fraction, end_fraction, replacement) in edits {
                    let boundaries: Vec<usize> = doc
                        .text()
                        .char_indices()
                        .map(|(i, _)| i)
                        .chain(std::iter::once(doc.text().len()))
                        .collect();
                    let start = boundaries[boundary_index(start_fraction, boundaries.len())];
                    let end = boundaries[boundary_index(end_fraction, boundaries.len())];
                    let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
                    let range = Range::new(
                        doc.line_index().position_at(lo),
                        doc.line_index().position_at(hi),
                    );
                    doc.apply_change(change(range, &replacement));
                }

                let fresh = Document::new(doc.text().to_string());
                prop_assert_eq!(doc.text(), fresh.text());
                prop_assert_eq!(doc.definitions(), fresh.definitions());
                prop_assert_eq!(doc.references(), fresh.references());
                prop_assert_eq!(doc.diagnostics(), fresh.diagnostics());
            }
        }
    }
}
