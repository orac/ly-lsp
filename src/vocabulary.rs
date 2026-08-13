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

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::command::{self, Command, definition::Variable};

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

/// One source of command definitions.
///
/// Layers are immutable once built. A file whose definitions change gets a
/// whole new `Layer`, with a new [`id`](Self::id), rather than being mutated
/// in place — which is what lets a scope be compared by its layers' identities
/// alone.
pub struct Layer {
    id: u64,
    commands: HashMap<String, Arc<dyn Command>>,
}

impl Layer {
    pub fn new(commands: HashMap<String, Arc<dyn Command>>) -> Self {
        Self {
            id: NEXT_LAYER_ID.fetch_add(1, Ordering::Relaxed),
            commands,
        }
    }

    /// The command this layer defines for `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Command>> {
        self.commands.get(name)
    }

    /// This layer's identity, unique among all layers ever built. See
    /// [`NEXT_LAYER_ID`].
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Every name this layer defines, in no particular order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.commands.keys().map(String::as_str)
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
            .field("commands", &self.commands.len())
            .finish()
    }
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
}

impl Scope {
    /// No layers at all.
    pub const EMPTY: Self = Self { top: None };

    /// This scope with `layer` above everything already in it, sharing all of
    /// it. Named for the environment model of evaluation, where a new frame is
    /// pushed by *extending* the environment it encloses.
    #[must_use]
    pub fn extended_with(&self, layer: Arc<Layer>) -> Self {
        Self {
            top: Some(Arc::new(Cell {
                layer,
                under: self.clone(),
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

    /// The command `\name` refers to: the nearest layer that has one wins.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Command>> {
        self.layers().find_map(|layer| layer.get(name))
    }

    /// Whether `\name` is a command we recognise at all. `name` is the command
    /// without its leading backslash.
    ///
    /// CamelCase names are accepted unconditionally: by LilyPond convention a
    /// `\Foo` with an uppercase initial is a context reference (a built-in like
    /// `\Staff` or a user-defined context), which the words file doesn't carry
    /// as a command. That stays a rule rather than a layer because it is one
    /// about the *shape* of a name — no map can hold the infinitely many
    /// `\Foo`s. The price is that a mistyped context name goes unflagged.
    pub fn is_known(&self, name: &str) -> bool {
        is_context_reference(name) || self.get(name).is_some()
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
/// Returns `None` if the words file can't be read, so the caller can leave
/// undefined-reference diagnostics switched off rather than flag everything.
/// The install layer is a lesser concern: if its directory can't be found or
/// read, the base still loads with an empty install layer — see
/// [`install::load`](crate::install::load).
pub fn workspace_base(words_path: &Path) -> Option<Scope> {
    let text = std::fs::read_to_string(words_path).ok()?;
    let mut base = Scope::EMPTY.extended_with(Arc::new(words_layer(&text)));
    if let Some(ly_dir) = crate::install::ly_dir(words_path) {
        base = base.extended_with(Arc::new(crate::install::load(&ly_dir)));
    }
    Some(base.extended_with(Arc::clone(&command::CURATED)))
}

/// The bottom layer: every name `lilypond-words` lists, with nothing behind it
/// but the fact that it exists.
///
/// Each becomes a [`Variable`] — a command with no arguments, so a block
/// after it is read as ordinary music.
fn words_layer(text: &str) -> Layer {
    let commands = parse_words(text)
        .chain(EXTRA_COMMANDS.iter().map(|name| (*name).to_string()))
        .map(|name| {
            let command = Arc::new(Variable::new(name.clone())) as Arc<dyn Command>;
            (name, command)
        })
        .collect();
    Layer::new(commands)
}

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
        Arc::new(crate::command::definition::layer(scheme::read(
            &crate::document::parse(&src, None),
            &src,
        )))
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
        let repeat = scope.get("repeat").expect("repeat is reserved");
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
        assert!(scope.get("break").expect("break").signature().is_empty());
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
        assert_eq!(scope.get("myFunc").expect("myFunc").signature().len(), 1);
    }

    #[test]
    fn the_nearest_file_layer_wins() {
        // Two files define `dup`, told apart by their arity; the first layer —
        // the document's own, or the nearest include — is the one that answers.
        let near = layer_defining("dup", 1);
        let far = layer_defining("dup", 2);
        let scope = Scope::EMPTY.for_document(&[Arc::clone(&near), Arc::clone(&far)]);
        assert_eq!(scope.get("dup").expect("dup").signature().len(), 1);
        let reversed = Scope::EMPTY.for_document(&[far, near]);
        assert_eq!(reversed.get("dup").expect("dup").signature().len(), 2);
    }

    #[test]
    fn a_reserved_word_outranks_a_file_that_binds_the_name() {
        // `\repeat` is a reserved word in LilyPond's own grammar: its parser
        // never reaches name lookup, so an assignment of that name can't
        // shadow it and ours must still win.
        let scope = Scope::builtins().for_document(&[layer_defining("repeat", 1)]);
        assert_eq!(scope.get("repeat").expect("repeat").signature().len(), 3);
    }

    #[test]
    fn a_file_outranks_a_curated_command_it_redefines() {
        // `\clef` is an ordinary `define-music-function` in LilyPond's own
        // `music-functions-init.ly`, so a file that binds `clef` really does
        // shadow it — and reporting our curated one-string signature for the
        // user's own two-argument `\clef` would be a lie.
        let scope = Scope::builtins().for_document(&[layer_defining("clef", 2)]);
        assert_eq!(scope.get("clef").expect("clef").signature().len(), 2);
    }

    #[test]
    fn a_fingerprint_follows_the_layers_stacked() {
        let one = layer_defining("a", 1);
        let two = layer_defining("b", 1);

        let plain = Scope::EMPTY.for_document(&[]).fingerprint();
        assert_eq!(
            Scope::EMPTY
                .for_document(&[Arc::new(Layer::new(HashMap::new()))])
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
}
