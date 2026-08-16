//! The set of commands ly-lsp recognises, as a stack of layers.
//!
//! A [`Layer`] is one *source* of command definitions: LilyPond's reserved words
//! ([`RESERVED`](crate::command::RESERVED)), our curated signatures for its
//! ordinary music functions ([`CURATED`](crate::command::CURATED)), the
//! definitions read out of one file, the active LilyPond install
//! ([`crate::install`]), and the bare names its `lilypond-words` file lists. A
//! [`Scope`] is a stack of those layers, and answers not just "is `\foo`
//! known?" but "what does it do?".
//!
//! Precedence is set by the order of the stack. Every scope is built
//! by [`Scope::for_document`], so the one place that says which layer outranks
//! which is that function. The order, top down:
//!
//! | Layer | Why there |
//! |---|---|
//! | [`RESERVED`](crate::command::RESERVED) | LilyPond's grammar recognises these before any name lookup happens, so nothing can rebind them |
//! | the document, then its includes, nearest first | a file that defines `\foo` means its own `\foo` |
//! | [`CURATED`](crate::command::CURATED) | our wording beats what the reader recovers from the install — but these are ordinary functions, and a file may shadow them |
//! | the install | what LilyPond itself defines |
//! | the words list | names with nothing behind them |
//!
//! A [`Scope`] is a persistent list: cloning shares every layer, and
//! [`extended_with`](Scope::extended_with) shares the whole tail. So the global
//! layers are built once at `initialize` and every document's scope is built
//! *from* that base rather than beside it.
//!
//! The layering is also what keeps a shared include parsed once rather than
//! once per file that includes it: a file's definitions are read when its
//! [`Document`](crate::document::Document) is built, and every scope that
//! reaches that file shares the same `Arc<Layer>`.
//!
//! See [`doc/command-parsing.md`](../doc/command-parsing.md) for the fuller
//! design.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::command::{self, Command, variable::Variable};
use crate::context::{ContextInstance, ContextType};

/// Commands that are valid but absent from `lilypond-words`, so we supply them
/// ourselves. `discant` is defined in Scheme by
/// `#(use-modules (lily accreg))`; we don't parse that definition, so we name
/// the command explicitly rather than chase it through the module.
const EXTRA_COMMANDS: &[&str] = &["discant"];

/// Hands out [`Layer::id`]s. Only distinctness matters, not the values: a
/// [`Scope`]'s fingerprint is a hash of the ids of its layers, so two scopes
/// agree exactly when they stack the same layer *instances*. Rebuilding a
/// file's layer (because the file was edited) mints a new id, which is what
/// makes every scope containing it compare unequal to what it was before, and
/// hence what re-analyses the documents that include it.
static NEXT_LAYER_ID: AtomicU64 = AtomicU64::new(0);

/// A name-keyed map, factored out because a [`Layer`] holds one of these per
/// *namespace* it recognises: commands, [`ContextType`]s, and
/// [`ContextInstance`] names (the `"vocals"` of `\new Voice = "vocals"`).
/// Lookup, emptiness and iteration read exactly the same regardless of what's
/// inside, so that logic is written once rather than once per namespace.
struct Table<T> {
    entries: HashMap<String, T>,
}

impl<T> Table<T> {
    fn new(entries: HashMap<String, T>) -> Self {
        Self { entries }
    }

    fn empty() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn get(&self, name: &str) -> Option<&T> {
        self.entries.get(name)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Every entry, with the name it's keyed under, in no particular order.
    fn iter(&self) -> impl Iterator<Item = (&str, &T)> {
        self.entries
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }
}

/// One source of definitions: commands, the [`ContextType`]s a file's
/// `\context { … }` blocks declare, and the [`ContextInstance`] names its
/// `\new`/`\context` calls create.
///
/// Layers are immutable once built. A file whose definitions change gets a
/// whole new `Layer`, with a new [`id`](Self::id), rather than being mutated
/// in place — which is what lets a scope be compared by its layers' identities
/// alone.
pub struct Layer {
    id: u64,
    origin: Arc<str>,
    commands: Table<Arc<dyn Command>>,
    context_types: Table<ContextType>,
    context_instances: Table<ContextInstance>,
}

impl Layer {
    /// `origin` is what to call the source these commands came from: a file
    /// name, `lilypond-2.24.3`, `lilypond-words`. See [`origin`](Self::origin).
    ///
    /// Commands only, no context types or instances — most call sites have
    /// nothing else to give. [`with_context_types`](Self::with_context_types)
    /// and [`with_context_instances`](Self::with_context_instances) add the
    /// other two namespaces where a caller has them.
    pub fn new(origin: impl Into<Arc<str>>, commands: HashMap<String, Arc<dyn Command>>) -> Self {
        Self {
            id: NEXT_LAYER_ID.fetch_add(1, Ordering::Relaxed),
            origin: origin.into(),
            commands: Table::new(commands),
            context_types: Table::empty(),
            context_instances: Table::empty(),
        }
    }

    /// Add `context_types` to `self`
    #[must_use]
    pub fn with_context_types(mut self, context_types: HashMap<String, ContextType>) -> Self {
        self.context_types = Table::new(context_types);
        self
    }

    /// Add `context_instances` to `self`
    #[must_use]
    pub fn with_context_instances(
        mut self,
        context_instances: HashMap<String, ContextInstance>,
    ) -> Self {
        self.context_instances = Table::new(context_instances);
        self
    }

    /// Where this layer's knowledge came from, to be shown to the reader —
    /// hover names it, so that `\foo` says whether it is the user's own, their
    /// LilyPond's, or merely a word in a list.
    ///
    /// Held here rather than on each [`Command`] because it is a property of
    /// the *source*, one per layer, and every command a layer holds shares it.
    /// A [`Scope`] lookup hands it back alongside the command it found, since a
    /// command on its own can't say which layer answered for it.
    pub fn origin(&self) -> &Arc<str> {
        &self.origin
    }

    /// The command this layer defines for `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Command>> {
        self.commands.get(name)
    }

    /// The context type this layer declares under `name`, if any.
    pub fn get_context_type(&self, name: &str) -> Option<&ContextType> {
        self.context_types.get(name)
    }

    /// The context instance this layer's `\new`/`\context` calls create under
    /// `name`, if any.
    pub fn get_context_instance(&self, name: &str) -> Option<&ContextInstance> {
        self.context_instances.get(name)
    }

    /// Whether this layer declares any context type at all. What
    /// [`Scope::has_context_types`] folds down the stack, once, at the point
    /// each layer is pushed — see there for why per-lookup isn't good enough.
    fn has_context_types(&self) -> bool {
        !self.context_types.is_empty()
    }

    /// This layer's identity, unique among all layers ever built. See
    /// [`NEXT_LAYER_ID`].
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether this layer defines nothing at all, in *any* of its
    /// namespaces. A layer that declares only a context type or instance —
    /// no commands — must still count as non-empty here:
    /// [`Scope::for_document`] skips empty layers and [`Scope::fingerprint`]
    /// hashes only non-empty ones, so an empty verdict would make the
    /// declaration invisible to every scope and never re-analysed on edit.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
            && self.context_types.is_empty()
            && self.context_instances.is_empty()
    }

    /// How many entries this layer holds, across all three namespaces.
    pub fn len(&self) -> usize {
        self.commands.len() + self.context_types.len() + self.context_instances.len()
    }

    /// Every command name this layer defines, in no particular order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.commands.names()
    }

    /// Every command this layer defines, with the name it answers to, in no
    /// particular order.
    pub fn commands(&self) -> impl Iterator<Item = (&str, &Arc<dyn Command>)> {
        self.commands.iter()
    }

    /// Every context type this layer declares, with the name it answers to,
    /// in no particular order.
    pub fn context_types(&self) -> impl Iterator<Item = (&str, &ContextType)> {
        self.context_types.iter()
    }

    /// Every context instance this layer's `\new`/`\context` calls create,
    /// with the name it answers to, in no particular order.
    pub fn context_instances(&self) -> impl Iterator<Item = (&str, &ContextInstance)> {
        self.context_instances.iter()
    }
}

// `Command` carries no `Debug` bound (it's an object-safe trait for dynamic
// dispatch, kept minimal), so `Arc<dyn Command>` isn't `Debug` either and the
// map can't be derived. `DocumentGraph` derives `Debug` and reaches a `Layer`
// through `Document`, so this stands in with the shape that matters for
// diagnosing a stuck server: which layer this is and how much it defines, not
// what each entry does.
impl std::fmt::Debug for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Layer")
            .field("id", &self.id)
            .field("origin", &self.origin)
            .field("commands", &self.commands.len())
            .field("context_types", &self.context_types.len())
            .field("context_instances", &self.context_instances.len())
            .finish()
    }
}

/// What a [`Scope`] knows about a name: the value it resolves to — a command,
/// a [`ContextType`] or a [`ContextInstance`] — and the [`Layer`] that
/// answered for it.
///
/// Generic over the value so one type serves every namespace a `Scope` can
/// look a name up in, rather than a `Known`-alike per namespace. The layer
/// travels with the value because the value can't say which one holds it —
/// the same `Arc<dyn Command>` may sit in several — and where a name was
/// found is part of the answer to "what is `\foo`?"; see [`Layer::origin`].
pub struct Known<'a, T> {
    pub value: &'a T,
    pub layer: &'a Arc<Layer>,
}

/// The commands visible from one document: layers, nearest first.
///
/// A persistent list. Cloning shares every layer and allocates nothing, and
/// [`extended_with`](Self::extended_with) shares the whole tail, so the base a
/// workspace loads once is the same memory every document's scope is built on.
#[derive(Clone, Default)]
pub struct Scope {
    top: Option<Arc<Cell>>,
}

/// One link of a [`Scope`]. `under` is a whole `Scope` rather than an
/// `Option<Arc<Cell>>` so that the tail of any scope is itself a scope,
/// shareable exactly as it stands.
struct Cell {
    layer: Arc<Layer>,
    under: Scope,
    /// Whether this layer or anything under it declares a context type.
    /// Folded in once, here, when the cell is built — see
    /// [`Scope::has_context_types`] for why that beats asking on every
    /// [`is_known`](Scope::is_known) call.
    has_context_types: bool,
}

impl Scope {
    /// No layers at all.
    pub const EMPTY: Self = Self { top: None };

    /// This scope with `layer` above everything already in it, sharing all of
    /// it. Named for the environment model of evaluation, where a new frame is
    /// pushed by *extending* the environment it encloses.
    #[must_use]
    pub fn extended_with(&self, layer: Arc<Layer>) -> Self {
        let has_context_types = layer.has_context_types() || self.has_context_types();
        Self {
            top: Some(Arc::new(Cell {
                layer,
                under: self.clone(),
                has_context_types,
            })),
        }
    }

    /// The hand-written base: our curated signatures alone, with no install
    /// and no words list behind them. What a workspace falls back to before
    /// (or without) a successful [`workspace_base`] load.
    pub fn builtins() -> Self {
        Self::EMPTY.extended_with(Arc::clone(&command::CURATED))
    }

    /// The scope a document is analysed in: `files` — the document's own layer
    /// first, then its includes, nearest first — stacked over this base, with
    /// LilyPond's reserved words pinned above the lot.
    ///
    /// Every scope in the server is built here, which is the point: the
    /// precedence rule is stated once, and no method that reads a scope has to
    /// know it. Empty layers are left out rather than stacked — a layer that
    /// defines nothing can't answer anything, and skipping it keeps a file
    /// that defines no commands fingerprinting the same however its scope was
    /// assembled.
    #[must_use]
    pub fn for_document(&self, files: &[Arc<Layer>]) -> Self {
        let mut scope = self.clone();
        for layer in files.iter().rev().filter(|layer| !layer.is_empty()) {
            scope = scope.extended_with(Arc::clone(layer));
        }
        scope.extended_with(Arc::clone(&command::RESERVED))
    }

    /// The scope with nothing but our hand-written knowledge — what a
    /// [`Document`](crate::document::Document) with no definitions of its own
    /// is first analysed in, before the graph knows what it can see.
    pub fn builtins_only() -> Self {
        Self::builtins().for_document(&[])
    }

    /// Every layer, nearest first.
    pub fn layers(&self) -> impl Iterator<Item = &Arc<Layer>> {
        std::iter::successors(self.top.as_deref(), |cell| cell.under.top.as_deref())
            .map(|cell| &cell.layer)
    }

    /// The nearest layer that answers `lookup` for a name, paired with the
    /// layer that answered — the shared machinery behind [`get`](Self::get)
    /// and [`get_context_type`](Self::get_context_type), so a third namespace
    /// won't need a third copy of "walk the layers, stop at the first hit".
    fn resolve<'a, T>(
        &'a self,
        lookup: impl Fn(&'a Layer) -> Option<&'a T>,
    ) -> Option<Known<'a, T>> {
        self.layers().find_map(|layer| {
            Some(Known {
                value: lookup(layer)?,
                layer,
            })
        })
    }

    /// What `\name` refers to: the nearest layer that has a command for it
    /// wins.
    pub fn get(&self, name: &str) -> Option<Known<'_, Arc<dyn Command>>> {
        self.resolve(|layer| layer.get(name))
    }

    /// What `Name` refers to: the nearest layer that declares a context type
    /// for it wins, the same shadowing rule [`get`](Self::get) follows for
    /// commands.
    pub fn get_context_type(&self, name: &str) -> Option<Known<'_, ContextType>> {
        self.resolve(|layer| layer.get_context_type(name))
    }

    /// What a `\change`, `\lyricsto` or bare `"name"` reference means: the
    /// nearest layer whose `\new`/`\context` calls create an instance called
    /// `name` wins, the same shadowing rule [`get`](Self::get) follows for
    /// commands. See [`ContextInstance`] for the file-scoped approximation
    /// this rests on.
    pub fn get_context_instance(&self, name: &str) -> Option<Known<'_, ContextInstance>> {
        self.resolve(|layer| layer.get_context_instance(name))
    }

    /// The shared machinery behind [`visible`](Self::visible) and
    /// [`visible_context_types`](Self::visible_context_types): every entry
    /// `entries` reads off a layer, for every layer, keeping only the first —
    /// nearest — one seen for each name.
    fn visible_via<'a, T, I>(
        &'a self,
        entries: impl Fn(&'a Layer) -> I,
    ) -> Vec<(&'a str, Known<'a, T>)>
    where
        I: Iterator<Item = (&'a str, &'a T)>,
    {
        let mut seen = HashSet::new();
        let mut visible = Vec::new();
        for layer in self.layers() {
            for (name, value) in entries(layer) {
                if seen.insert(name) {
                    visible.push((name, Known { value, layer }));
                }
            }
        }
        visible
    }

    /// Everything this scope can resolve, with the name each command answers
    /// to: what [`get`](Self::get) says, for every name at once, which is what
    /// a completion list is. A name bound in more than one layer appears
    /// once, from the nearest — the same shadowing rule `get` follows, so the
    /// list can never offer a `\foo` that means something else once written.
    ///
    /// A `Vec` rather than an iterator because remembering which names have
    /// already been answered for takes state the caller has no use for, and
    /// the caller wants the whole lot anyway.
    pub fn visible(&self) -> Vec<(&str, Known<'_, Arc<dyn Command>>)> {
        self.visible_via(Layer::commands)
    }

    /// Every context type this scope can resolve, with the name it answers
    /// to — the [`visible`](Self::visible) of the context-type namespace, for
    /// its own completion list, with the same de-duplication.
    pub fn visible_context_types(&self) -> Vec<(&str, Known<'_, ContextType>)> {
        self.visible_via(Layer::context_types)
    }

    /// Every context instance this scope can resolve, with the name it
    /// answers to — the [`visible`](Self::visible) of the context-instance
    /// namespace, for its own completion list, with the same
    /// de-duplication.
    pub fn visible_context_instances(&self) -> Vec<(&str, Known<'_, ContextInstance>)> {
        self.visible_via(Layer::context_instances)
    }

    /// Whether this scope's layers — install, workspace, or both — declare
    /// any [`ContextType`] at all.
    ///
    /// Computed once per layer, when it's pushed by
    /// [`extended_with`](Self::extended_with), and folded down the stack from
    /// there, so asking is reading one `bool` off the top [`Cell`] rather than
    /// walking every layer. That matters because [`is_known`](Self::is_known)
    /// asks it once per reference in a document.
    fn has_context_types(&self) -> bool {
        self.top.as_ref().is_some_and(|cell| cell.has_context_types)
    }

    /// Whether `\name` is a command or context type we recognise at all.
    /// `name` is written without its leading backslash.
    ///
    /// A real hit in either namespace always counts. Failing that, a
    /// CamelCase name — by LilyPond convention, `\Foo` with an uppercase
    /// initial is a context reference — is accepted too, but *only* as a
    /// **fallback for a scope that knows no context types whatsoever**. This
    /// is not a rule about what a well-typed context reference looks like;
    /// it exists because the install can fail to load (no share directory
    /// passed, an unreadable directory, a version layout this reader doesn't
    /// recognise), and when it does, the context-type namespace is empty
    /// everywhere in scope. Refusing every CamelCase name at that point would
    /// flag `\Staff` in every score a workspace opens, which is a worse
    /// outcome than the fallback's actual price: a genuinely mistyped
    /// `\Vioce` goes unflagged only while the scope has no real context types
    /// to check it against. Once any layer supplies even one, the fallback
    /// stops applying and a typo like `\Vioce` is rejected on its own merits.
    pub fn is_known(&self, name: &str) -> bool {
        self.get(name).is_some()
            || self.get_context_type(name).is_some()
            || (!self.has_context_types() && is_context_reference(name))
    }

    /// A value identifying which layers this scope stacks, so an analysis can
    /// record the scope it was made in and be redone only when that changes.
    /// Two scopes share a fingerprint exactly when they stack the same
    /// *contentful* layer instances in the same order.
    ///
    /// Empty layers are skipped rather than hashed, for the reason
    /// [`for_document`](Self::for_document) gives for not stacking them: a
    /// base assembled with an install that couldn't be read must fingerprint
    /// as one assembled without it.
    pub fn fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        for layer in self.layers().filter(|layer| !layer.is_empty()) {
            layer.id().hash(&mut hasher);
        }
        hasher.finish()
    }
}

impl std::fmt::Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.layers()).finish()
    }
}

/// The layers every document in a workspace shares, loaded once: the names
/// LilyPond's `lilypond-words` file lists at the bottom, the definitions read
/// out of the same installation over them, and our curated signatures on top.
///
/// `share_dir` is LilyPond's version-specific share directory — the one
/// whose children include `vim/syntax/lilypond-words` and `ly/` — as passed
/// by the client at `initialize`, so both are found by joining onto it
/// directly rather than working back up from either.
///
/// Returns `Err` if the words file can't be read, so the caller can leave
/// undefined-reference diagnostics switched off rather than flag everything,
/// while still reporting why.
/// The install layer is a lesser concern: if its directory can't be found or
/// read, the base still loads with an empty install layer — see
/// [`install::load`](crate::install::load).
pub fn workspace_base(share_dir: &Path) -> std::io::Result<Scope> {
    let words_path = share_dir.join("vim").join("syntax").join("lilypond-words");
    let text = std::fs::read_to_string(words_path)?;
    let base = Scope::EMPTY
        .extended_with(Arc::new(words_layer(&text)))
        .extended_with(Arc::new(crate::install::load(&share_dir.join("ly"))));
    Ok(base.extended_with(Arc::clone(&command::CURATED)))
}

/// The bottom layer: every name `lilypond-words` lists, with nothing behind it
/// but the fact that it exists.
///
/// Each becomes a [`Variable`] — a command with no arguments, so a block
/// after it is read as ordinary music.
fn words_layer(text: &str) -> Layer {
    let commands: HashMap<String, Arc<dyn Command>> = parse_words(text)
        .chain(EXTRA_COMMANDS.iter().map(|name| (*name).to_string()))
        .map(|name| {
            let command = Arc::new(Variable::new(name.clone())) as Arc<dyn Command>;
            (name, command)
        })
        .collect();
    Layer::new(WORDS_ORIGIN, commands)
}

/// What hover calls the words layer: the name of the file it is read from, and
/// as good a summary of what it knows as any — a name and nothing else.
const WORDS_ORIGIN: &str = "lilypond-words";

/// Whether `name` looks like a context reference, i.e. its first character is an
/// uppercase letter.
fn is_context_reference(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// Parses a `lilypond-words` file into command names. Command entries carry a
/// doubled leading backslash (`\\relative`); context, grob and engraver names
/// (`Staff`, `NoteHead`, `Note_heads_engraver`) have none and are dropped.
fn parse_words(text: &str) -> impl Iterator<Item = String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix(r"\\"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::scheme;
    use crate::line_struct::Span;

    /// A layer defining one command of the given name, taking `arity`
    /// arguments, read from a real definition so the test exercises the same
    /// path the server does.
    fn layer_defining(name: &str, arity: usize) -> Arc<Layer> {
        let args = (0..arity)
            .map(|i| format!("m{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let predicates = vec!["ly:music?"; arity].join(" ");
        let src = format!("{name} = #(define-music-function ({args}) ({predicates}) #{{ #}})\n");
        Arc::new(crate::command::definition::layer(
            scheme::read(&crate::document::parse(&src, None), &src),
            Arc::from(src.as_str()),
            Arc::from("test.ly"),
        ))
    }

    /// A `ContextType` with nothing but a name — enough for the lookup and
    /// shadowing tests, which don't care about aliases, descriptions or
    /// spans.
    fn context_type(name: &str) -> ContextType {
        ContextType {
            name: name.to_string(),
            aliases: Vec::new(),
            description: None,
            name_span: Span::new(0, 0),
            block_span: Span::new(0, 0),
        }
    }

    /// A layer holding only the given context type, no commands at all — the
    /// shape a `\layout { \context { \name Foo } }` with nothing else in the
    /// file produces.
    fn layer_declaring(name: &str) -> Arc<Layer> {
        let mut context_types = HashMap::new();
        context_types.insert(name.to_string(), context_type(name));
        Arc::new(Layer::new("test.ly", HashMap::new()).with_context_types(context_types))
    }

    /// A `ContextInstance` with nothing but a name — enough for the lookup
    /// and shadowing tests below, which don't care about the type it was
    /// created as or its span.
    fn context_instance(name: &str) -> ContextInstance {
        ContextInstance {
            name: name.to_string(),
            type_name: None,
            span: Span::new(0, 0),
        }
    }

    /// A layer holding only the given context instance, no commands at all —
    /// the shape a bare `\new Voice = "vocals" { … }` with nothing else in
    /// the file produces.
    fn layer_creating(name: &str) -> Arc<Layer> {
        let mut context_instances = HashMap::new();
        context_instances.insert(name.to_string(), context_instance(name));
        Arc::new(Layer::new("test.ly", HashMap::new()).with_context_instances(context_instances))
    }

    /// A base holding only the given words, as a workspace with no install
    /// would have.
    fn words(text: &str) -> Scope {
        Scope::EMPTY
            .extended_with(Arc::new(words_layer(text)))
            .extended_with(Arc::clone(&command::CURATED))
    }

    #[test]
    fn parses_commands_and_drops_context_names() {
        // Commands carry a doubled backslash and are kept with both stripped;
        // context/grob names without a backslash are dropped.
        let layer = words_layer("\\\\relative\n\\\\new\nStaff\nNoteHead\n\\\\score\n");
        assert!(layer.get("relative").is_some());
        assert!(layer.get("new").is_some());
        assert!(layer.get("score").is_some());
        assert!(layer.get("Staff").is_none());
        assert!(layer.get("NoteHead").is_none());
    }

    #[test]
    fn the_words_layer_is_named_for_the_file_it_read() {
        let layer = words_layer("\\\\relative\n");
        assert_eq!(layer.origin().as_ref(), "lilypond-words");
    }

    #[test]
    fn a_lookup_says_which_layer_answered() {
        // Two layers define `dup`; the one that wins is the one hover names.
        let scope = words("").for_document(&[layer_defining("dup", 1)]);
        let known = scope.get("dup").expect("dup");
        assert!(Arc::ptr_eq(
            known.layer,
            scope.layers().nth(1).expect("file")
        ));
    }

    #[test]
    fn extras_are_known_even_when_absent_from_words() {
        let scope = words("\\\\relative\n").for_document(&[]);
        assert!(scope.is_known("relative"));
        assert!(scope.is_known("discant"));
    }

    #[test]
    fn builtin_commands_are_known_even_when_absent_from_words_and_extras() {
        // `with` used to need listing among the extras; now it's answered by
        // the reserved layer instead, without appearing in either source.
        let scope = words("\\\\relative\n").for_document(&[]);
        assert!(scope.is_known("with"));
        assert!(scope.get("with").is_some());
    }

    #[test]
    fn camelcase_commands_are_accepted_as_context_references() {
        let scope = words("\\\\relative\n").for_document(&[]);
        // Built-in and user-defined contexts alike, without being in the words.
        assert!(scope.is_known("Staff"));
        assert!(scope.is_known("MyOwnContext"));
        // Lowercase commands still have to be known.
        assert!(!scope.is_known("wibble"));
    }

    #[test]
    fn get_resolves_a_reserved_command() {
        let scope = Scope::builtins_only();
        let repeat = scope.get("repeat").expect("repeat is reserved").value;
        assert_eq!(repeat.name(), "repeat");
        assert_eq!(repeat.signature().len(), 3);
    }

    #[test]
    fn a_bare_words_name_resolves_to_a_command_taking_no_arguments() {
        // All the words list says is that the name exists, so it resolves to a
        // variable: a call to it consumes nothing, and any block after it is
        // read as ordinary music.
        let scope = words("\\\\break\n").for_document(&[]);
        assert!(scope.is_known("break"));
        assert!(
            scope
                .get("break")
                .expect("break")
                .value
                .signature()
                .is_empty()
        );
    }

    #[test]
    fn get_returns_none_for_an_unknown_name() {
        let scope = words("\\\\relative\n").for_document(&[]);
        assert!(scope.get("wibble").is_none());
    }

    #[test]
    fn a_file_layer_defines_a_command_the_base_never_heard_of() {
        let scope = words("").for_document(&[layer_defining("myFunc", 1)]);
        assert!(scope.is_known("myFunc"));
        assert_eq!(
            scope.get("myFunc").expect("myFunc").value.signature().len(),
            1
        );
    }

    #[test]
    fn a_shadowed_name_is_listed_once_by_the_layer_that_wins() {
        // What a completion list must not do: offer `\dup` twice, one of them
        // a signature no call in this document would ever get.
        let near = layer_defining("dup", 1);
        let far = layer_defining("dup", 2);
        let scope = Scope::EMPTY.for_document(&[near, far]);
        let dups: Vec<_> = scope
            .visible()
            .into_iter()
            .filter(|(name, _)| *name == "dup")
            .collect();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].1.value.signature().len(), 1);
    }

    #[test]
    fn the_nearest_file_layer_wins() {
        // Two files define `dup`, told apart by their arity; the first layer —
        // the document's own, or the nearest include — is the one that answers.
        let near = layer_defining("dup", 1);
        let far = layer_defining("dup", 2);
        let scope = Scope::EMPTY.for_document(&[Arc::clone(&near), Arc::clone(&far)]);
        assert_eq!(scope.get("dup").expect("dup").value.signature().len(), 1);
        let reversed = Scope::EMPTY.for_document(&[far, near]);
        assert_eq!(reversed.get("dup").expect("dup").value.signature().len(), 2);
    }

    #[test]
    fn a_reserved_word_outranks_a_file_that_binds_the_name() {
        // `\repeat` is a reserved word in LilyPond's own grammar: its parser
        // never reaches name lookup, so an assignment of that name can't
        // shadow it and ours must still win.
        let scope = Scope::builtins().for_document(&[layer_defining("repeat", 1)]);
        assert_eq!(
            scope.get("repeat").expect("repeat").value.signature().len(),
            3
        );
    }

    #[test]
    fn a_file_outranks_a_curated_command_it_redefines() {
        // `\clef` is an ordinary `define-music-function` in LilyPond's own
        // `music-functions-init.ly`, so a file that binds `clef` really does
        // shadow it — and reporting our curated one-string signature for the
        // user's own two-argument `\clef` would be a lie.
        let scope = Scope::builtins().for_document(&[layer_defining("clef", 2)]);
        assert_eq!(scope.get("clef").expect("clef").value.signature().len(), 2);
    }

    #[test]
    fn a_fingerprint_follows_the_layers_stacked() {
        let one = layer_defining("a", 1);
        let two = layer_defining("b", 1);

        let plain = Scope::EMPTY.for_document(&[]).fingerprint();
        assert_eq!(
            Scope::EMPTY
                .for_document(&[Arc::new(Layer::new("empty", HashMap::new()))])
                .fingerprint(),
            plain,
            "an empty layer is no layer, however the scope was built"
        );
        assert_ne!(
            Scope::EMPTY.for_document(&[Arc::clone(&one)]).fingerprint(),
            plain
        );
        assert_eq!(
            Scope::EMPTY.for_document(&[Arc::clone(&one)]).fingerprint(),
            Scope::EMPTY.for_document(&[Arc::clone(&one)]).fingerprint(),
            "the same layer stacked twice over fingerprints the same"
        );
        assert_ne!(
            Scope::EMPTY
                .for_document(&[Arc::clone(&one), Arc::clone(&two)])
                .fingerprint(),
            Scope::EMPTY.for_document(&[two, one]).fingerprint(),
            "order matters: it decides which definition wins"
        );
    }

    #[test]
    fn rebuilding_a_layer_changes_the_fingerprint() {
        // The same source read twice gives two layers with the same contents
        // but different identities — which is exactly what makes an edited
        // include re-analyse its dependants.
        let before = Scope::EMPTY
            .for_document(&[layer_defining("a", 1)])
            .fingerprint();
        let after = Scope::EMPTY
            .for_document(&[layer_defining("a", 1)])
            .fingerprint();
        assert_ne!(before, after);
    }

    #[test]
    fn a_scope_shares_the_tail_it_was_extended_from() {
        // The point of the persistent list: extending a base doesn't copy it,
        // so the layers a workspace loads once are the same instances every
        // document's scope reads.
        let base = words("\\\\break\n");
        let scope = base.for_document(&[layer_defining("myFunc", 1)]);
        for (from_base, from_scope) in base.layers().zip(scope.layers().skip(2)) {
            assert!(Arc::ptr_eq(from_base, from_scope));
        }
    }

    #[test]
    fn a_context_type_resolves_through_a_scope() {
        let scope = Scope::EMPTY.for_document(&[layer_declaring("Staff")]);
        let known = scope.get_context_type("Staff").expect("Staff");
        assert_eq!(known.value.name, "Staff");
    }

    #[test]
    fn a_nearer_layer_shadows_a_further_ones_context_type() {
        // Same shadowing rule as `get` for commands: two files declare
        // `MyStaff` differently (told apart here just by which one answers),
        // and the nearer — the document's own, or the nearest include — wins.
        let near = layer_declaring("MyStaff");
        let far = layer_declaring("MyStaff");
        let scope = Scope::EMPTY.for_document(&[Arc::clone(&near), Arc::clone(&far)]);
        let known = scope.get_context_type("MyStaff").expect("MyStaff");
        assert!(Arc::ptr_eq(known.layer, &near));
    }

    #[test]
    fn visible_context_types_deduplicates_a_shadowed_name() {
        let near = layer_declaring("MyStaff");
        let far = layer_declaring("MyStaff");
        let scope = Scope::EMPTY.for_document(&[near, far]);
        let matches: Vec<_> = scope
            .visible_context_types()
            .into_iter()
            .filter(|(name, _)| *name == "MyStaff")
            .collect();
        assert_eq!(matches.len(), 1, "a shadowed context type is offered once");
    }

    #[test]
    fn a_layer_with_only_a_context_type_reports_non_empty() {
        // Load-bearing: `for_document` skips empty layers and `fingerprint`
        // hashes only non-empty ones, so a layer with a context type but no
        // commands must not be mistaken for one with nothing at all — or its
        // declaration silently vanishes from every scope that stacks it and
        // is never re-analysed on edit.
        let layer = layer_declaring("Staff");
        assert!(!layer.is_empty());
        assert_eq!(layer.len(), 1);

        // And it must actually survive `for_document`, not just claim to be
        // non-empty in isolation.
        let scope = Scope::EMPTY.for_document(&[Arc::clone(&layer)]);
        assert!(
            scope.layers().any(|l| Arc::ptr_eq(l, &layer)),
            "a context-type-only layer must not be skipped as if it were empty"
        );
    }

    #[test]
    fn two_scopes_differing_only_by_a_context_type_only_layer_fingerprint_differently() {
        let plain = Scope::EMPTY.for_document(&[]).fingerprint();
        let with_context_type = Scope::EMPTY
            .for_document(&[layer_declaring("Staff")])
            .fingerprint();
        assert_ne!(
            plain, with_context_type,
            "a layer declaring only a context type must still change the fingerprint"
        );
    }

    #[test]
    fn a_context_instance_resolves_through_a_scope() {
        let scope = Scope::EMPTY.for_document(&[layer_creating("vocals")]);
        let known = scope.get_context_instance("vocals").expect("vocals");
        assert_eq!(known.value.name, "vocals");
    }

    #[test]
    fn a_nearer_layer_shadows_a_further_ones_context_instance() {
        // Same shadowing rule as `get` for commands and `get_context_type`
        // for context types: the nearer — the document's own, or the
        // nearest include — wins.
        let near = layer_creating("vocals");
        let far = layer_creating("vocals");
        let scope = Scope::EMPTY.for_document(&[Arc::clone(&near), Arc::clone(&far)]);
        let known = scope.get_context_instance("vocals").expect("vocals");
        assert!(Arc::ptr_eq(known.layer, &near));
    }

    #[test]
    fn visible_context_instances_deduplicates_a_shadowed_name() {
        let near = layer_creating("vocals");
        let far = layer_creating("vocals");
        let scope = Scope::EMPTY.for_document(&[near, far]);
        let matches: Vec<_> = scope
            .visible_context_instances()
            .into_iter()
            .filter(|(name, _)| *name == "vocals")
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "a shadowed context instance is offered once"
        );
    }

    #[test]
    fn a_layer_with_only_a_context_instance_reports_non_empty() {
        // Load-bearing for the same reason as the context-type case above:
        // an instance-only layer must not vanish from every scope that
        // stacks it.
        let layer = layer_creating("vocals");
        assert!(!layer.is_empty());
        assert_eq!(layer.len(), 1);

        let scope = Scope::EMPTY.for_document(&[Arc::clone(&layer)]);
        assert!(
            scope.layers().any(|l| Arc::ptr_eq(l, &layer)),
            "a context-instance-only layer must not be skipped as if it were empty"
        );
    }

    #[test]
    fn two_scopes_differing_only_by_a_context_instance_only_layer_fingerprint_differently() {
        let plain = Scope::EMPTY.for_document(&[]).fingerprint();
        let with_context_instance = Scope::EMPTY
            .for_document(&[layer_creating("vocals")])
            .fingerprint();
        assert_ne!(
            plain, with_context_instance,
            "a layer creating only a context instance must still change the fingerprint"
        );
    }
}
