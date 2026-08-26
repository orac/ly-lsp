//! Note-name spellings per language, read from the LilyPond installation's own `scm/lily/define-note-names.scm`.
//!
//! LilyPond's note names are language-dependent and we resolve pitches lexically, so we need exactly the tables its parser uses — for the version the workspace is actually running, not for whichever version happened to be around when this file was written. So this is a reader, not a table: [`NoteNames::read`] parses the installation's own data, the same way [`install`](crate::install) reads its `.ly` files for commands.
//!
//! Two files are read, both under `scm/lily`:
//!
//! - `define-note-names.scm`, whose `language-pitch-names` alist holds one block per language (`(nederlands . ((ceses . ,(ly:make-pitch -1 0 DOUBLE-FLAT)) …))`) and whose tail aliases some of them (`català` is also `catalan`, `deutsch` is also `semi-german`).
//! - `lily-library.scm`, for what the alteration constants those entries name are worth. They are rationals in whole tones (`FLAT` is `-1/2`), which we scale to the quarter-tone integers a [`Language`] stores.
//!
//! Reading the constants rather than knowing them is what lets a language whose alterations go beyond the usual nine — 2.24's `arabic`, with its `FIVE-HALF-FLAT` — come through whole rather than half-parsed.
//!
//! A [`Language`] is a cheap handle on one language's table, held in a [`MusicContext`](crate::command::MusicContext) and passed around by the note analyser as the language in force. The whole set belongs to the install [`Layer`](crate::vocabulary::Layer) that read it, and a [`Scope`](crate::vocabulary::Scope) hands out the nearest one.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock};

/// The language LilyPond's parser starts in, before any `\language` or language include.
///
/// Named here because `define-note-names.scm` doesn't say so — the default is wired into the parser, not into the data — and looked up by name so that it survives the file being reordered. [`NoteNames::parse`] falls back to whichever language the file declares first if a future version drops or renames this one.
const DEFAULT_LANGUAGE: &str = "nederlands";

/// Quarter tones to the whole tone, the unit LilyPond writes its alteration constants in (`DOUBLE-FLAT` is `-1`, a whole tone flat) and the factor that converts them to the quarter-tone integers a [`Language`] stores.
const QUARTER_TONES_PER_WHOLE_TONE: i32 = 4;

/// Every note-name language one LilyPond installation knows, by every name it answers to.
///
/// Aliases share their canonical language's table rather than copying it, so `\language "català"` and `\language "catalan"` yield the same [`Language`], reporting the same [`name`](Language::name).
#[derive(Debug)]
pub struct NoteNames {
    /// Keyed by every accepted spelling, canonical and alias alike.
    languages: HashMap<String, Language>,
    /// The language in force before anything selects one. See [`DEFAULT_LANGUAGE`].
    default: Language,
}

/// The registry for a workspace with no readable installation: it knows no language at all, and its [`default_language`](NoteNames::default_language) knows no spellings.
///
/// Shared rather than rebuilt, so that [`Scope::note_names`](crate::vocabulary::Scope::note_names) can hand out a reference without every scope owning one.
static NONE_AT_ALL: LazyLock<NoteNames> = LazyLock::new(|| NoteNames {
    languages: HashMap::new(),
    default: Language(Arc::new(LanguageNames {
        name: String::new(),
        table: Vec::new(),
    })),
});

/// One language's note names: a cheap, shareable handle on a table read out of the install.
///
/// Cloning bumps a refcount, which is what lets a [`MusicContext`](crate::command::MusicContext) carry the language in force without the analyser copying a table per command call.
///
/// Two languages are equal when they have the same [`name`](Self::name). That is the question the analyser actually asks — "did this `\language` change anything?" — and it makes an alias equal to what it aliases, since both report the canonical name.
#[derive(Clone)]
pub struct Language(Arc<LanguageNames>);

/// One row of a language's table: a spelling, its diatonic note name (`0 = c` … `6 = b`), and its alteration in quarter-tone steps (sharp `+2`, flat `-2`).
type Spelling = (String, u8, i8);

/// A language part-way through being read: the name its header gave, and the spellings gathered under it so far.
type PartialLanguage = (String, Vec<Spelling>);

/// The table behind a [`Language`], shared by every handle on it.
#[derive(Debug)]
struct LanguageNames {
    /// The canonical name, as `define-note-names.scm` writes it — `català`, not the `catalan` alias.
    name: String,
    /// Sorted by spelling, so [`Language::note`] can binary-search it.
    table: Vec<Spelling>,
}

impl NoteNames {
    /// Reads the note-name languages out of `share_dir`, LilyPond's version-specific share directory (`share/lilypond/2.24.3`) — the same directory [`install::load`](crate::install::load) reads its `.ly` files from.
    ///
    /// A file that can't be read leaves the whole set empty rather than failing: like the install layer, we would rather offer a degraded service than none, and [`empty`](Self::empty) degrades quietly — the analyser stops claiming to know what is and isn't a note rather than flagging every note in the score.
    pub fn read(share_dir: &Path) -> NoteNames {
        let scm_dir = share_dir.join("scm").join("lily");
        let (Ok(names), Ok(constants)) = (
            std::fs::read_to_string(scm_dir.join("define-note-names.scm")),
            std::fs::read_to_string(scm_dir.join("lily-library.scm")),
        ) else {
            return NoteNames::empty();
        };
        NoteNames::parse(&names, &constants)
    }

    /// Parses the text of `define-note-names.scm` and of `lily-library.scm`, in that order. Split from [`read`](Self::read) so a test can supply a fixture without a directory to put it in.
    pub fn parse(note_names: &str, constants: &str) -> NoteNames {
        let alterations = parse_alterations(constants);
        let mut languages: HashMap<String, Language> = HashMap::new();
        // Declaration order, to name the fallback default and to keep the parse a single pass.
        let mut declared: Vec<String> = Vec::new();
        let mut current: Option<PartialLanguage> = None;

        for line in note_names.lines() {
            if let Some(name) = language_header(line) {
                finish(current.take(), &mut languages, &mut declared);
                current = Some((name.to_string(), Vec::new()));
            } else if let Some((spelling, note, alteration)) = pitch_entry(line, &alterations)
                && let Some((_, table)) = current.as_mut()
            {
                table.push((spelling.to_string(), note, alteration));
            }
        }
        finish(current.take(), &mut languages, &mut declared);

        // The aliases come after every language block, so every canonical name they can name is already known.
        for line in note_names.lines() {
            for (canonical, alias) in bare_pairs(line) {
                if let Some(language) = languages.get(canonical).cloned() {
                    languages.insert(alias.to_string(), language);
                }
            }
        }

        let default = languages
            .get(DEFAULT_LANGUAGE)
            .or_else(|| declared.first().and_then(|name| languages.get(name)))
            .cloned()
            .unwrap_or_else(|| NONE_AT_ALL.default.clone());
        NoteNames { languages, default }
    }

    /// The set a workspace with no readable installation gets. See [`NONE_AT_ALL`].
    pub fn empty() -> NoteNames {
        NoteNames {
            languages: HashMap::new(),
            default: NONE_AT_ALL.default.clone(),
        }
    }

    /// The shared empty set, for a caller that has a reference to hand out rather than a value to build.
    pub fn none_at_all() -> &'static NoteNames {
        &NONE_AT_ALL
    }

    /// The language a `\language "name"` selects, or `None` if it names none this installation knows. Aliases resolve to the language they alias.
    pub fn language(&self, name: &str) -> Option<Language> {
        self.languages.get(name).cloned()
    }

    /// The language in force before anything selects one — Dutch, in every version so far. See [`DEFAULT_LANGUAGE`].
    pub fn default_language(&self) -> Language {
        self.default.clone()
    }

    /// Every spelling `\language` accepts, with the language it selects, in alphabetical order. Aliases are listed alongside what they alias, since either is a valid thing to write.
    pub fn accepted_names(&self) -> Vec<(&str, &Language)> {
        let mut names: Vec<(&str, &Language)> = self
            .languages
            .iter()
            .map(|(name, language)| (name.as_str(), language))
            .collect();
        names.sort_by_key(|&(name, _)| name);
        names
    }

    /// Whether no language was read at all — the degraded state [`empty`](Self::empty) leaves behind.
    pub fn is_empty(&self) -> bool {
        self.languages.is_empty()
    }
}

impl Language {
    /// The canonical name of this language, as `define-note-names.scm` writes it. An alias reports what it aliases, so `\language "catalan"` and `\language "català"` both name `català`.
    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// Whether this language knows no spellings at all — what
    /// [`NoteNames::default_language`] answers with when there is no
    /// installation to read. Callers use it to tell "this symbol is not a
    /// note" from "we have no idea what a note looks like here", which are
    /// very different things to report to a reader.
    pub fn is_empty(&self) -> bool {
        self.0.table.is_empty()
    }

    /// Resolves a note-name spelling to its `(note name, alteration)`, or `None` if this language has no such note.
    pub fn note(&self, spelling: &str) -> Option<(u8, i8)> {
        self.0
            .table
            .binary_search_by(|(name, _, _)| name.as_str().cmp(spelling))
            .ok()
            .map(|i| {
                let (_, note, alteration) = self.0.table[i];
                (note, alteration)
            })
    }

    /// A spelling for the pitch `(note name, alteration)` in this language, or `None` if it has no name for it. Where a language gives several spellings (English `ef` and `e-flat`), the shortest is chosen, so callers get the terse form a writer would use rather than the spelled-out alias.
    pub fn spell(&self, note_name: u8, alteration: i8) -> Option<&str> {
        self.0
            .table
            .iter()
            .filter(|(_, note, alteration_here)| {
                *note == note_name && *alteration_here == alteration
            })
            .map(|(name, _, _)| name.as_str())
            .min_by_key(|name| name.len())
    }

    /// This language's seven naturals, in diatonic order (`c` … `b`), as it spells them — `do, re, mi, …` for the Romance languages, `c, d, e, … h` for the Germanic ones. What completion shows to tell one language from another, since nothing in the data describes a language but the names it gives.
    pub fn naturals(&self) -> Vec<&str> {
        (0..7).filter_map(|note| self.spell(note, 0)).collect()
    }
}

impl PartialEq for Language {
    fn eq(&self, other: &Self) -> bool {
        self.0.name == other.0.name
    }
}

impl Eq for Language {}

impl std::fmt::Debug for Language {
    /// The name alone: a table of a hundred spellings in every `{:?}` of a `MusicContext` would bury whatever the reader was actually looking at.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Language({:?})", self.0.name)
    }
}

/// Files away the language just finished, keeping its table sorted by spelling for [`Language::note`]'s binary search and dropping any duplicate spelling in favour of the first — which is LilyPond's own resolution, its parser reading the alist front to back.
fn finish(
    language: Option<PartialLanguage>,
    languages: &mut HashMap<String, Language>,
    declared: &mut Vec<String>,
) {
    let Some((name, mut table)) = language else {
        return;
    };
    table.sort_by(|a, b| a.0.cmp(&b.0));
    table.dedup_by(|a, b| a.0 == b.0);
    declared.push(name.clone());
    languages.insert(
        name.clone(),
        Language(Arc::new(LanguageNames { name, table })),
    );
}

/// The language a block header opens, if `line` is one: `    (nederlands . (`, and nothing else in the file has that shape.
fn language_header(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix('(')?;
    let name = rest.strip_suffix(" . (")?;
    (!name.is_empty() && !name.contains(char::is_whitespace)).then_some(name)
}

/// The `(spelling . ,(ly:make-pitch -1 <note> <ALTERATION>))` entry on `line`, resolved through the alteration constants read from `lily-library.scm`. `None` for any other line, and for an entry naming an alteration that file doesn't define.
fn pitch_entry<'a>(line: &'a str, alterations: &HashMap<String, i8>) -> Option<(&'a str, u8, i8)> {
    let (spelling, rest) = line.trim().strip_prefix('(')?.split_once(" . ")?;
    let arguments = rest.trim().strip_prefix(",(ly:make-pitch ")?;
    let mut fields = arguments.trim_end_matches(')').split_whitespace();
    // The octave is always -1: these tables spell pitches, and the octave a written note lands in comes from its marks, not from here.
    fields.next()?;
    let note = fields.next()?.parse().ok()?;
    let alteration = *alterations.get(fields.next()?)?;
    (note < 7).then_some((spelling, note, alteration))
}

/// Every `(one two)` pair of bare words on `line` — the shape the alias list at the tail of `define-note-names.scm` is written in, whether it puts one pair per line (2.26) or all of them on one (2.24).
///
/// Deliberately shapeless: a caller keeps only the pairs whose first word already names a language, which is what stops an incidental two-word form elsewhere in the file (`(string->symbol str)`) being taken for an alias.
fn bare_pairs(line: &str) -> impl Iterator<Item = (&str, &str)> {
    line.split('(').filter_map(|piece| {
        let (pair, _) = piece.split_once(')')?;
        let (first, second) = pair.trim().split_once(' ')?;
        let bare = |word: &str| {
            !word.is_empty()
                && !word.contains(['(', ')', '\'', '.', ','])
                && !word.contains(char::is_whitespace)
        };
        let second = second.trim();
        (bare(first) && bare(second)).then_some((first, second))
    })
}

/// Reads `lily-library.scm`'s `(define-public NAME <rational>)` constants, keeping those whose value is a whole number of quarter tones — every alteration constant, and nothing else that matters here (`CENTER 0` and friends come along harmlessly, under names no pitch entry ever writes).
fn parse_alterations(constants: &str) -> HashMap<String, i8> {
    constants
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("(define-public ")?;
            let (name, value) = rest.trim_end_matches(')').trim().split_once(' ')?;
            Some((name.to_string(), quarter_tones(value.trim())?))
        })
        .collect()
}

/// A whole-tone alteration written as a Scheme rational (`-3/4`, `1`, `0`) in quarter tones, or `None` if it isn't a rational or doesn't land on a quarter tone.
fn quarter_tones(value: &str) -> Option<i8> {
    let (numerator, denominator) = match value.split_once('/') {
        Some((numerator, denominator)) => (numerator, denominator.parse::<i32>().ok()?),
        None => (value, 1),
    };
    let scaled = numerator.parse::<i32>().ok()? * QUARTER_TONES_PER_WHOLE_TONE;
    (denominator != 0 && scaled % denominator == 0)
        .then(|| i8::try_from(scaled / denominator).ok())
        .flatten()
}

/// LilyPond's alteration constants, enough of `lily-library.scm` for a test fixture to resolve the twelve-tone spellings [`ENGLISH_FIXTURE`] uses.
#[cfg(test)]
pub(crate) const CONSTANTS_FIXTURE: &str = "\
(define-public DOUBLE-FLAT  -1)
(define-public FLAT -1/2)
(define-public NATURAL 0)
(define-public SHARP 1/2)
(define-public DOUBLE-SHARP 1)
";

/// A stand-in for `define-note-names.scm`: English names, twelve-tone only, which is all the unit tests around here need to write a note and read it back.
///
/// Being the only language it declares, it is also the default the analyser starts in — so a test written against this fixture writes `cs` and `ef`, not `cis` and `ees`. Anything that turns on a language *switch*, on a quarter tone, or on a spelling only some languages have belongs in an integration test reading a real installation, where the tables are LilyPond's own rather than this abbreviation of them.
#[cfg(test)]
pub(crate) const ENGLISH_FIXTURE: &str = "\
(define-session-public language-pitch-names
  `(
    (english . (
                (cff . ,(ly:make-pitch -1 0 DOUBLE-FLAT))
                (cf . ,(ly:make-pitch -1 0 FLAT))
                (c . ,(ly:make-pitch -1 0 NATURAL))
                (cs . ,(ly:make-pitch -1 0 SHARP))
                (css . ,(ly:make-pitch -1 0 DOUBLE-SHARP))

                (dff . ,(ly:make-pitch -1 1 DOUBLE-FLAT))
                (df . ,(ly:make-pitch -1 1 FLAT))
                (d . ,(ly:make-pitch -1 1 NATURAL))
                (ds . ,(ly:make-pitch -1 1 SHARP))
                (dss . ,(ly:make-pitch -1 1 DOUBLE-SHARP))

                (eff . ,(ly:make-pitch -1 2 DOUBLE-FLAT))
                (ef . ,(ly:make-pitch -1 2 FLAT))
                (e . ,(ly:make-pitch -1 2 NATURAL))
                (es . ,(ly:make-pitch -1 2 SHARP))
                (ess . ,(ly:make-pitch -1 2 DOUBLE-SHARP))

                (fff . ,(ly:make-pitch -1 3 DOUBLE-FLAT))
                (ff . ,(ly:make-pitch -1 3 FLAT))
                (f . ,(ly:make-pitch -1 3 NATURAL))
                (fs . ,(ly:make-pitch -1 3 SHARP))
                (fss . ,(ly:make-pitch -1 3 DOUBLE-SHARP))

                (gff . ,(ly:make-pitch -1 4 DOUBLE-FLAT))
                (gf . ,(ly:make-pitch -1 4 FLAT))
                (g . ,(ly:make-pitch -1 4 NATURAL))
                (gs . ,(ly:make-pitch -1 4 SHARP))
                (gss . ,(ly:make-pitch -1 4 DOUBLE-SHARP))

                (aff . ,(ly:make-pitch -1 5 DOUBLE-FLAT))
                (af . ,(ly:make-pitch -1 5 FLAT))
                (a . ,(ly:make-pitch -1 5 NATURAL))
                (as . ,(ly:make-pitch -1 5 SHARP))
                (ass . ,(ly:make-pitch -1 5 DOUBLE-SHARP))

                (bff . ,(ly:make-pitch -1 6 DOUBLE-FLAT))
                (bf . ,(ly:make-pitch -1 6 FLAT))
                (b . ,(ly:make-pitch -1 6 NATURAL))
                (bs . ,(ly:make-pitch -1 6 SHARP))
                (bss . ,(ly:make-pitch -1 6 DOUBLE-SHARP))
                ))
    ))
";

/// The fixture set, shared by every unit test in the crate that needs to resolve a pitch. Built once: parsing it per test would be cheap, but a shared set makes every test's [`Language`] the same one, as it is in a real document.
#[cfg(test)]
pub(crate) fn fixture() -> Arc<NoteNames> {
    static FIXTURE: LazyLock<Arc<NoteNames>> =
        LazyLock::new(|| Arc::new(NoteNames::parse(ENGLISH_FIXTURE, CONSTANTS_FIXTURE)));
    Arc::clone(&FIXTURE)
}

/// The one language [`fixture`] holds, for a test that needs a [`Language`] to hand to a parser rather than a whole set.
#[cfg(test)]
pub(crate) fn fixture_language() -> Language {
    fixture().default_language()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn english() -> Language {
        fixture().language("english").expect("english")
    }

    #[test]
    fn the_only_language_a_file_declares_is_its_default() {
        // The fixture has no `nederlands` to be the default, so the first language declared stands in — the rule that keeps a set usable however LilyPond reorganises the file.
        assert_eq!(fixture().default_language(), english());
    }

    #[test]
    fn spellings_resolve_to_note_name_and_alteration() {
        assert_eq!(english().note("cs"), Some((0, 2)));
        assert_eq!(english().note("ef"), Some((2, -2)));
        assert_eq!(english().note("bff"), Some((6, -4)));
        assert_eq!(
            english().note("h"),
            None,
            "`h` is a German name, not English"
        );
    }

    #[test]
    fn an_unreadable_installation_leaves_a_language_that_knows_nothing() {
        let empty = NoteNames::empty();
        assert!(empty.is_empty());
        let language = empty.default_language();
        assert!(
            language.is_empty(),
            "so that callers can tell `not a note` from `no idea what a note is`"
        );
        assert_eq!(language.note("c"), None);
    }

    #[test]
    fn alteration_constants_are_read_in_quarter_tones() {
        let alterations = parse_alterations(
            "(define-public DOUBLE-FLAT  -1)\n(define-public THREE-Q-FLAT -3/4)\n(define-public SEMI-SHARP 1/4)\n(define-public FIVE-HALF-FLAT -5/2)\n",
        );
        assert_eq!(alterations.get("DOUBLE-FLAT"), Some(&-4));
        assert_eq!(alterations.get("THREE-Q-FLAT"), Some(&-3));
        assert_eq!(alterations.get("SEMI-SHARP"), Some(&1));
        // Beyond the usual nine, and read anyway: 2.24's `arabic` writes it.
        assert_eq!(alterations.get("FIVE-HALF-FLAT"), Some(&-10));
    }

    #[test]
    fn an_alias_shares_its_languages_table_and_reports_its_name() {
        let names = NoteNames::parse(
            &format!("{ENGLISH_FIXTURE}\n '((english inglese) (nonesuch nothing))"),
            CONSTANTS_FIXTURE,
        );
        let alias = names.language("inglese").expect("the alias resolves");
        assert_eq!(alias, english());
        assert_eq!(
            alias.name(),
            "english",
            "an alias answers to its canonical name"
        );
        assert_eq!(
            names.language("nothing"),
            None,
            "a pair naming no language is not an alias, whatever else it may be"
        );
    }

    #[test]
    fn naturals_are_the_languages_own_spelling_of_c_to_b() {
        assert_eq!(english().naturals(), ["c", "d", "e", "f", "g", "a", "b"]);
    }

    proptest! {
        /// Spelling a pitch and reading the spelling back must return the pitch you started with — otherwise a refactoring that writes a spelled note wouldn't be able to trust its own output.
        #[test]
        fn spell_and_note_round_trip(note_name in 0u8..7, alteration in -4i8..=4) {
            let language = english();
            let Some(spelling) = language.spell(note_name, alteration) else {
                // The fixture is twelve-tone, so it names no quarter tone; nothing to round-trip.
                return Ok(());
            };
            prop_assert_eq!(language.note(spelling), Some((note_name, alteration)));
        }
    }
}
