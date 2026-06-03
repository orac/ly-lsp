#!/usr/bin/env python3
"""Generate src/note_names.rs from LilyPond's scm/define-note-names.scm.

LilyPond's note-name spellings are language-dependent, and we resolve pitches
lexically, so we need the same tables it uses. Rather than transcribe them by
hand we parse the canonical source.

Usage:
    python tools/gen_note_names.py /path/to/lilypond/scm/define-note-names.scm > src/note_names.rs
"""

import re
import sys

# Alteration constants from scm/lily-library.scm, in whole-tone units, scaled to
# the quarter-tone integer unit we store (a semitone, FLAT/SHARP, is +-2).
ALTERATION = {
    "DOUBLE-FLAT": -4,
    "THREE-Q-FLAT": -3,
    "FLAT": -2,
    "SEMI-FLAT": -1,
    "NATURAL": 0,
    "SEMI-SHARP": 1,
    "SHARP": 2,
    "THREE-Q-SHARP": 3,
    "DOUBLE-SHARP": 4,
}

# (canonical-key . alias) pairs appended at the end of the scm file.
ALIASES = {
    "català": "catalan",
    "español": "espanol",
    "deutsch": "semi-german",
    "português": "portugues",
}

# Map every accepted `\language`/`\include` spelling to a Rust enum variant.
# Accented forms are how the scm file names them; ASCII forms are common in
# include filenames and the historic aliases.
VARIANTS = {
    "nederlands": "Nederlands",
    "català": "Catalan",
    "catalan": "Catalan",
    "deutsch": "Deutsch",
    "semi-german": "SemiGerman",
    "english": "English",
    "español": "Espanol",
    "espanol": "Espanol",
    "français": "Francais",
    "francais": "Francais",
    "italiano": "Italiano",
    "norsk": "Norsk",
    "português": "Portugues",
    "portugues": "Portugues",
    "suomi": "Suomi",
    "svenska": "Svenska",
    "vlaams": "Vlaams",
}

LANG_HEADER = re.compile(r"^\s*\((\S+) \. \(\s*$")
ENTRY = re.compile(r"\((\S+) \. ,\(ly:make-pitch -1 (\d+) ([A-Z0-9-]+)\)\)")


def parse(path):
    """Returns {language-key: [(name, note, alteration), ...]} in file order."""
    langs = {}
    current = None
    with open(path, encoding="utf-8") as f:
        for line in f:
            header = LANG_HEADER.match(line)
            if header and header.group(1) in (
                set(VARIANTS) | set(ALIASES)
            ):
                current = header.group(1)
                langs[current] = []
                continue
            entry = ENTRY.search(line)
            if entry and current is not None:
                name, note, alt = entry.groups()
                langs[current].append((name, int(note), ALTERATION[alt]))
    for canonical, alias in ALIASES.items():
        langs[alias] = langs[canonical]
    return langs


def emit(langs):
    out = []
    w = out.append
    w("//! Note-name spellings per language, generated from LilyPond's")
    w("//! `scm/define-note-names.scm` by `tools/gen_note_names.py`. Do not edit by")
    w("//! hand; re-run the generator against a LilyPond checkout instead.")
    w("//!")
    w("//! Each table maps a spelling to its diatonic note name (`0 = c` … `6 = b`)")
    w("//! and alteration in quarter-tone steps (sharp `+2`, flat `-2`).")
    w("")
    w("/// A note-name language selectable with `\\language` or by including the")
    w("/// matching `.ly` file. [`Language::Nederlands`] is LilyPond's default.")
    w("#[derive(Debug, Clone, Copy, PartialEq, Eq)]")
    w("pub enum Language {")
    for variant in sorted(set(VARIANTS.values())):
        w(f"    {variant},")
    w("}")
    w("")
    w("impl Language {")
    w("    /// LilyPond's default note-name language, in force until a")
    w("    /// `\\language` or language include changes it.")
    w("    pub const DEFAULT: Language = Language::Nederlands;")
    w("")
    w("    /// The language a `\\language \"name\"` string or a `name`(`.ly`) include")
    w("    /// selects, or `None` if it names no known language.")
    w("    pub fn from_name(name: &str) -> Option<Language> {")
    w("        Some(match name {")
    for spelling in sorted(VARIANTS):
        w(f'            {rust_str(spelling)} => Language::{VARIANTS[spelling]},')
    w("            _ => return None,")
    w("        })")
    w("    }")
    w("")
    w("    /// The note-name table for this language, sorted by spelling.")
    w("    fn table(self) -> &'static [(&'static str, u8, i8)] {")
    w("        match self {")
    # One table per variant; pick the first language key that maps to it.
    emitted = {}
    for key, variant in [(k, VARIANTS[k]) for k in VARIANTS]:
        if variant in emitted or key not in langs:
            continue
        emitted[variant] = key
    for variant in sorted(emitted):
        w(f"            Language::{variant} => {const_name(variant)},")
    w("        }")
    w("    }")
    w("")
    w("    /// Resolves a note-name spelling to its `(note name, alteration)`, or")
    w("    /// `None` if this language has no such note.")
    w("    pub fn note(self, spelling: &str) -> Option<(u8, i8)> {")
    w("        self.table()")
    w("            .binary_search_by_key(&spelling, |&(name, _, _)| name)")
    w("            .ok()")
    w("            .map(|i| {")
    w("                let (_, note, alt) = self.table()[i];")
    w("                (note, alt)")
    w("            })")
    w("    }")
    w("}")
    w("")
    for variant in sorted(emitted):
        key = emitted[variant]
        entries = sorted(set(langs[key]), key=lambda e: e[0])
        w(f"#[rustfmt::skip]")
        w(f"static {const_name(variant)}: &[(&str, u8, i8)] = &[")
        for name, note, alt in entries:
            w(f"    ({rust_str(name)}, {note}, {alt}),")
        w("];")
        w("")
    return "\n".join(out)


def const_name(variant):
    # CamelCase variant -> SCREAMING_SNAKE constant.
    s = re.sub(r"(?<!^)(?=[A-Z])", "_", variant).upper()
    return f"{s}_NAMES"


def rust_str(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    langs = parse(sys.argv[1])
    print(emit(langs))


if __name__ == "__main__":
    main()
