# Documentation from the manual

Hover currently shows what the *install* knows: a command's signature, and the docstring on its `define-…-function` where it has one. That is a lot for the ~190 functions defined that way, and nothing at all for the several hundred commands defined elsewhere — and even where there is a docstring, it is the one-paragraph note the implementer left, not the section of the Notation Reference that explains the thing.

This is a plan, not an implementation. Nothing described here is built.

The goal, smallest useful version first:

1. **A link.** Hover ends with *See “Tuplets” in the Notation Reference*, pointing at the exact anchor for that command.
2. **A snippet before the link.** The first paragraph or two of that section, converted to Markdown, so the answer is in the hover and the link is for going deeper.

Naming note: this document and the eventual module are called *manual hover* for want of better. It is really "documentation lookup against the published manual", and a better name is welcome before any of it is written down in code.

## Where the documentation is, and isn't

**Not in the install.** Checked on Windows against three installs — 2.24.1, 2.24.3 and 2.26.0. Each has `bin`, `lib`, `libexec` and `share/{emacs,guile,lilypond,locale,man}`; no `share/doc`, no `share/info`, and a `share/man/man1` that exists but is empty. Across 109 MB there is not one `.texi`, `.info`, `.html` or `.pdf`. The Windows installer offers no option to add them. So the read-the-install approach that every other layer uses has nothing to read, and this is the one kind of knowledge that has to come from somewhere else.

**Published as a separate download.** `lilypond-<version>-documentation.tar.xz`, on the GitLab release alongside the binaries — 166 MB compressed for 2.24.1. That is the built website: split HTML, big HTML, and PDF. Too big to bundle and too big to ask a user to fetch for the sake of a hover.

**Published as a website**, `lilypond.org/doc/v<major>.<minor>/`, which is the same HTML from that tarball, served per version. A single index page is 0.4–1.2 MB and a single section page around 200 KB. That is the source to use.

## Fetch on first use, cache on disk

Bundling a pre-built index in the extension was considered and rejected:

- It would have to be built per LilyPond version, and shipped again whenever a new LilyPond comes out — coupling our release cadence to theirs, for data that is not ours.
- The manuals are licensed under the GNU FDL. Redistributing them inside the extension raises a licensing question that fetching at runtime does not raise at all.

So: on the first hover that wants it, download the index for the install's version, store it under the server's cache directory, and use the cache from then on. Section pages likewise, lazily, so a user only ever pulls the handful their hovers actually touch. Offline, or before the fetch completes, hover degrades to what it shows today plus a plain link.

Consequences to design for:

- **The version is the cache key.** The server already knows the install's version; map `2.24.x` → `/doc/v2.24/`. Two installs on one machine mean two cached indexes.
- **Fetching must never block a hover.** Return the signature immediately and the manual text when it arrives, or on the next hover.
- **A failed fetch is normal**, not an error to report: no network, a proxy, a version whose docs aren't published. Fall back and stay quiet.

## The two documentation structures

The output changed between 2.24 and 2.26 — LilyPond regenerated the manuals with a newer Texinfo, and the HTML is different enough to need two readers. Both were inspected on lilypond.org in August 2026.

### The command index

| | 2.24 | 2.26 |
|---|---|---|
| Path under `Documentation/notation/` | `lilypond-command-index.html` | `index-of-commands-and-concepts.html` |
| Size | 439 KB | 1.2 MB |
| Contents | commands only (concepts are in a separate `lilypond-index.html`) | commands **and** concepts, merged |
| Columns per row | 3 | 4 (a spare `<td>&nbsp;</td>`) |
| Section title | `Tuplets` | `2.1.2 Tuplets` |

The 2.24 path 404s under `/doc/v2.26/`, so the fetcher needs both names and a fallback between them.

A row in each, for `\tuplet`:

```html
<!-- 2.24 -->
<tr><td></td><td valign="top"><a href="writing-rhythms#index-_005ctuplet-1"><code>\tuplet</code></a></td><td valign="top"><a href="writing-rhythms#tuplets">Tuplets</a></td></tr>

<!-- 2.26 -->
<tr><td></td><td valign="top"><a href="writing-rhythms#index-_005ctuplet"><code>\tuplet</code></a></td><td>&nbsp;</td><td valign="top"><a href="writing-rhythms#tuplets">2.1.2 Tuplets</a></td></tr>
```

Both give the three things a link needs: the page, the anchor within it, and the human-readable section title. Command entries are the ones whose text is wrapped in `<code>` and begins with a backslash, which is how they separate from the concept entries mixed in with them in 2.26. Strip the leading section number from 2.26's titles.

One command appears several times — `\tuplet` is indexed under *Tuplets*, *Polymetric notation*, and the *Available music functions* appendix. The first is the one worth linking; the appendix entry never is, since it is generated from the same docstring the install already gave us.

### The section pages

The anchors are the reason this needs care. Texinfo 6 wrote `<a name="…">`; Texinfo 7 writes `<span id="…">`, and it dropped the numeric suffix on this particular anchor:

```html
<!-- 2.24: writing-rhythms.html -->
<a name="tuplets"></a>
<h4 class="unnumberedsubsubsec">Tuplets</h4>
<a name="index-tuplet"></a>
<a name="index-_005ctuplet"></a>
<a name="index-_005ctuplet-1"></a>
<p>Tuplets are made from a music expression with the <code>\tuplet</code>

<!-- 2.26: writing-rhythms.html -->
<div class="subsection" id="tuplets">
…
<h4 class="subsection">2.1.2 Tuplets</h4>
<span id="index-tuplet"></span>
<span id="index-_005ctuplet"></span>
<p>Tuplets are made from a music expression with the <code>\tuplet</code>
```

**Read anchors from the index; never construct them from the command name.** `index-_005ctuplet-1` in one version and `index-_005ctuplet` in the other is exactly the drift that would break a constructed anchor, and the index already carries the right one for its own version.

Other differences a snippet extractor meets:

| | 2.24 | 2.26 |
|---|---|---|
| Index anchor element | `<a name="…"></a>` | `<span id="…"></span>` |
| Section container | none; a flat run of siblings | `<div class="subsection" id="…">` |
| Heading class | `unnumberedsubsubsec` | `subsection`, with the number in the text |
| `@example` block | `<blockquote><pre class="example">` | `<div class="example"><pre class="example">` |
| `@lilypond[verbatim]` block | `<blockquote><pre class="verbatim">` | same |
| Chapter numbering | Tuplets is 1.2.1 | Tuplets is 2.1.2 |

Every section page also opens with a `<table class="nav_table">` of previous/next links, which must be skipped.

The extraction wanted is: from the command's index anchor, take the following siblings up to the next heading, and stop earlier at a `Predefined commands`, `Selected snippets` or `See also` subheading — those are the parts of a section that are useless out of context. Then convert to Markdown. The `<pre class="verbatim">` blocks are already syntax-highlighted into `<span>`s, so their text content, fenced as `lilypond`, gives back the source the reader wrote.

## Why not the Texinfo sources

Tempting, because [`src/command/scheme.rs`](../src/command/scheme.rs) already converts Texinfo to Markdown, and the sources say precisely which node documents which command:

```texinfo
@node Tuplets
@unnumberedsubsubsec Tuplets
@funindex \tuplet

Tuplets are made from a music expression with the @code{\tuplet}
command, multiplying the speed of the music expression by a fraction:
```

They live at `Documentation/en/notation/*.itely` in the source tree, reachable per version as `https://gitlab.com/lilypond/lilypond/-/raw/v2.24.1/…` — 113 KB for the rhythms chapter, and no HTML parsing at all.

Reject it anyway. The converter we have handles the *closed* subset of Texinfo that docstrings use — a dozen inline commands and three block environments, counted across every docstring in the install. The manual uses the whole language: `@node`, `@ref`/`@rinternals`/`@rlearning` cross-references, `@table`, `@itemize`, `@lilypondfile`, `@include`, and the macros defined in `common-macros.itexi`, which have to be expanded before anything else can be read. Following the sources means writing a real Texinfo processor, which is the thing we decided not to do. The published HTML is that processor's output: macros expanded, cross-references resolved to hrefs, examples highlighted.

The `@funindex` lines are still worth knowing about as a cross-check if the HTML index ever proves incomplete.

## Open questions

- **One index or several?** The command index covers the Notation Reference. The Learning Manual and the Internals Reference have their own, and `\override`-style questions are often better answered by Internals. Start with Notation.
- **How much to inline.** First paragraph is a guess; it may want to be "up to the first example block, or 400 characters".
- **Where the cache lives**, and whether the extension or the server owns the fetch. The server knows the version and answers the hover; the extension has VS Code's HTTP proxy settings, which corporate networks need.
- **Whether a fetch needs consent.** The server making network requests is new behaviour for this extension, and probably wants a setting, defaulting to on, and a mention in the README.
