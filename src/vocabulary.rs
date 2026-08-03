//! The set of commands ly-lsp recognises, as a stack of layers.
//!
//! A [`Layer`] is one *source* of command knowledge: the hand-written
//! [`BUILTIN`](crate::command::BUILTIN) table, the definitions read out of one
//! file, and (from step 4) the active LilyPond install. A [`Scope`] is the
//! stack of layers visible from one document — its own definitions, those of
//! everything it `\include`s, and the global [`Vocabulary`] underneath — and
//! answers not just "is `\foo` known?" but "what does it do?".
//!
//! The layering is what keeps a shared include parsed once rather than once
//! per file that includes it: a file's definitions are read when its
//! [`Document`](crate::document::Document) is built, and every scope that
//! reaches that file borrows the same `Arc<Layer>`.
//!
//! See [`doc/command-parsing.md`](../doc/command-parsing.md) for the fuller
//! design.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::command::{self, Command};

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

/// One source of command definitions: the hand-written builtins, the
/// `define-music-function`s of a single file, or the active LilyPond install.
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

/// The global, document-independent part of the vocabulary: the names
/// LilyPond's own `lilypond-words` file lists, and (from step 4) the
/// definitions read out of the active install.
///
/// Context references (`\Staff`, `\PianoStaff`, and user-defined contexts) are
/// recognised by their CamelCase initial rather than stored here: the words
/// file lists context names without a backslash, indistinguishable from the
/// grob and engraver names we *don't* want to accept as commands.
#[derive(Debug)]
pub struct Vocabulary {
    /// Read from the active LilyPond install. Empty until step 4.
    install: Arc<Layer>,
    /// Names from `lilypond-words` with nothing behind them. Known, but with
    /// no signature: [`Scope::get`] doesn't resolve them, so a call to one is
    /// left unparsed and its following block read as ordinary music, exactly
    /// as before any of this existed.
    known_names: HashSet<String>,
}

impl Vocabulary {
    /// Loads the vocabulary from a `lilypond-words` file, folding in our extras.
    ///
    /// Returns `None` if the file can't be read, so the caller can leave
    /// undefined-reference diagnostics switched off rather than flag everything.
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        Some(Self::from_words(&text))
    }

    /// Builds a vocabulary from the contents of a `lilypond-words` file.
    fn from_words(text: &str) -> Self {
        let mut known_names = parse_words(text);
        known_names.extend(EXTRA_COMMANDS.iter().map(|s| (*s).to_string()));
        Self {
            install: Arc::new(Layer::new(HashMap::new())),
            known_names,
        }
    }

    /// A [`Scope`] resolving through this vocabulary with no file layers — what
    /// a document with no definitions of its own and no includes sees.
    pub fn scope(&self) -> Scope<'_> {
        Scope::new(Some(self), Vec::new())
    }
}

/// The commands visible from one document: the hand-written builtins, the
/// definitions of the document and everything it includes, and the global
/// [`Vocabulary`] underneath.
///
/// Resolution order is `builtin` → file layers, nearest first → `install`, and
/// finally the bare `known_names`, which [`is_known`](Self::is_known) accepts
/// but [`get`](Self::get) does not resolve. `builtin` outranks the rest because
/// it exists precisely where they are absent or unhelpful — `\repeat` and
/// friends are reserved words in LilyPond's own grammar, not functions a
/// Scheme reader could ever discover — and a user's own definitions outrank
/// the install's because a user redefining `\foo` means theirs.
pub struct Scope<'a> {
    vocabulary: Option<&'a Vocabulary>,
    /// The file layers, in include-closure order: the document itself first,
    /// then what it includes, so the nearest definition wins.
    files: Vec<Arc<Layer>>,
}

impl<'a> Scope<'a> {
    pub fn new(vocabulary: Option<&'a Vocabulary>, files: Vec<Arc<Layer>>) -> Self {
        Self { vocabulary, files }
    }

    /// The scope with nothing but the hand-written builtins — what a
    /// [`Document`](crate::document::Document) is first analysed in, before the
    /// graph knows which files it can see.
    pub const fn builtins_only() -> Scope<'static> {
        Scope {
            vocabulary: None,
            files: Vec::new(),
        }
    }

    /// The command `\name` refers to, or `None` where nothing defines one with
    /// a signature. A bare `known_names` entry resolves to `None` here on
    /// purpose: knowing a name exists says nothing about its arguments, and
    /// [`command::parse`] declining the call is what leaves a following block
    /// to be read as ordinary music.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Command>> {
        command::BUILTIN
            .get(name)
            .or_else(|| self.files.iter().find_map(|layer| layer.get(name)))
            .or_else(|| self.vocabulary.and_then(|v| v.install.get(name)))
    }

    /// Whether `\name` is a command we recognise at all. `name` is the command
    /// without its leading backslash.
    ///
    /// CamelCase names are accepted unconditionally: by LilyPond convention a
    /// `\Foo` with an uppercase initial is a context reference (a built-in like
    /// `\Staff` or a user-defined context), which the words file doesn't carry
    /// as a command. The price is that a mistyped context name goes unflagged.
    pub fn is_known(&self, name: &str) -> bool {
        is_context_reference(name)
            || self.get(name).is_some()
            || self
                .vocabulary
                .is_some_and(|v| v.known_names.contains(name))
    }

    /// A value identifying which layers this scope stacks, so an analysis can
    /// record the scope it was made in and be redone only when that changes.
    /// Two scopes share a fingerprint exactly when they stack the same
    /// *contentful* layer instances in the same order.
    ///
    /// Empty layers are skipped rather than hashed. A layer that defines
    /// nothing resolves nothing, so it cannot change an analysis, and passing
    /// over it means a file that defines no commands (and includes none that
    /// do) fingerprints the same however its scope was assembled — which is
    /// what keeps the overwhelming majority of documents from being analysed a
    /// second time for no gain.
    pub fn fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        for layer in self
            .vocabulary
            .map(|v| &v.install)
            .into_iter()
            .chain(&self.files)
            .filter(|layer| !layer.is_empty())
        {
            layer.id().hash(&mut hasher);
        }
        hasher.finish()
    }
}

impl std::fmt::Debug for Scope<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope")
            .field("vocabulary", &self.vocabulary)
            .field("files", &self.files)
            .finish()
    }
}

/// Whether `name` looks like a context reference, i.e. its first character is an
/// uppercase letter.
fn is_context_reference(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// Parses a `lilypond-words` file into command names. Command entries carry a
/// doubled leading backslash (`\\relative`); context, grob and engraver names
/// (`Staff`, `NoteHead`, `Note_heads_engraver`) have none and are dropped.
fn parse_words(text: &str) -> HashSet<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix(r"\\"))
        .map(str::to_string)
        .collect()
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
        Arc::new(scheme::read(&crate::document::parse(&src, None), &src).layer)
    }

    #[test]
    fn parses_commands_and_drops_context_names() {
        // Commands carry a doubled backslash and are kept with both stripped;
        // context/grob names without a backslash are dropped.
        let words = "\\\\relative\n\\\\new\nStaff\nNoteHead\n\\\\score\n";
        let commands = parse_words(words);
        assert!(commands.contains("relative"));
        assert!(commands.contains("new"));
        assert!(commands.contains("score"));
        assert!(!commands.contains("Staff"));
        assert!(!commands.contains("NoteHead"));
        assert_eq!(commands.len(), 3);
    }

    #[test]
    fn extras_are_known_even_when_absent_from_words() {
        let vocab = Vocabulary::from_words("\\\\relative\n");
        assert!(vocab.scope().is_known("relative"));
        assert!(vocab.scope().is_known("discant"));
    }

    #[test]
    fn builtin_commands_are_known_even_when_absent_from_words_and_extras() {
        // `with` used to need listing among the extras; now it's answered by
        // the builtin layer instead, without appearing in either source.
        let vocab = Vocabulary::from_words("\\\\relative\n");
        assert!(vocab.scope().is_known("with"));
        assert!(vocab.scope().get("with").is_some());
    }

    #[test]
    fn camelcase_commands_are_accepted_as_context_references() {
        let vocab = Vocabulary::from_words("\\\\relative\n");
        // Built-in and user-defined contexts alike, without being in the words.
        assert!(vocab.scope().is_known("Staff"));
        assert!(vocab.scope().is_known("MyOwnContext"));
        // Lowercase commands still have to be known.
        assert!(!vocab.scope().is_known("wibble"));
    }

    #[test]
    fn get_resolves_a_builtin_command() {
        let scope = Scope::builtins_only();
        let repeat = scope.get("repeat").expect("repeat is builtin");
        assert_eq!(repeat.name(), "repeat");
        assert_eq!(repeat.signature().len(), 3);
    }

    #[test]
    fn a_bare_known_name_is_known_but_resolves_to_no_command() {
        let vocab = Vocabulary::from_words("\\\\break\n");
        assert!(vocab.scope().is_known("break"));
        assert!(vocab.scope().get("break").is_none());
    }

    #[test]
    fn get_returns_none_for_an_unknown_name() {
        let vocab = Vocabulary::from_words("\\\\relative\n");
        assert!(vocab.scope().get("wibble").is_none());
    }

    #[test]
    fn a_file_layer_defines_a_command_the_vocabulary_never_heard_of() {
        let vocab = Vocabulary::from_words("");
        let scope = Scope::new(Some(&vocab), vec![layer_defining("myFunc", 1)]);
        assert!(scope.is_known("myFunc"));
        assert_eq!(scope.get("myFunc").expect("myFunc").signature().len(), 1);
    }

    #[test]
    fn the_nearest_file_layer_wins() {
        // Two files define `dup`, told apart by their arity; the first layer —
        // the document's own, or the nearest include — is the one that answers.
        let near = layer_defining("dup", 1);
        let far = layer_defining("dup", 2);
        let scope = Scope::new(None, vec![Arc::clone(&near), Arc::clone(&far)]);
        assert_eq!(scope.get("dup").expect("dup").signature().len(), 1);
        let reversed = Scope::new(None, vec![far, near]);
        assert_eq!(reversed.get("dup").expect("dup").signature().len(), 2);
    }

    #[test]
    fn builtins_outrank_a_file_that_redefines_one() {
        // `\repeat` is a reserved word in LilyPond's own grammar: an assignment
        // of that name can't reach it, so ours must still win.
        let scope = Scope::new(None, vec![layer_defining("repeat", 1)]);
        assert_eq!(scope.get("repeat").expect("repeat").signature().len(), 3);
    }

    #[test]
    fn a_fingerprint_follows_the_layers_stacked() {
        let one = layer_defining("a", 1);
        let two = layer_defining("b", 1);

        let plain = Scope::builtins_only().fingerprint();
        assert_eq!(
            Scope::new(None, vec![]).fingerprint(),
            plain,
            "no layers is no layers, however it was built"
        );
        assert_ne!(
            Scope::new(None, vec![Arc::clone(&one)]).fingerprint(),
            plain
        );
        assert_eq!(
            Scope::new(None, vec![Arc::clone(&one)]).fingerprint(),
            Scope::new(None, vec![Arc::clone(&one)]).fingerprint(),
            "the same layer stacked twice over fingerprints the same"
        );
        assert_ne!(
            Scope::new(None, vec![Arc::clone(&one), Arc::clone(&two)]).fingerprint(),
            Scope::new(None, vec![two, one]).fingerprint(),
            "order matters: it decides which definition wins"
        );
    }

    #[test]
    fn rebuilding_a_layer_changes_the_fingerprint() {
        // The same source read twice gives two layers with the same contents
        // but different identities — which is exactly what makes an edited
        // include re-analyse its dependants.
        let before = Scope::new(None, vec![layer_defining("a", 1)]).fingerprint();
        let after = Scope::new(None, vec![layer_defining("a", 1)]).fingerprint();
        assert_ne!(before, after);
    }
}
