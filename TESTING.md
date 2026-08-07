# Testing

Run all the tests with `cargo test`. Alongside the unit tests in `src/`, there are integration tests in `tests/`: some ordinary Rust ones, some file-based suites with their own little formats, and some that read a real LilyPond installation.

## Tests need a real LilyPond installation

The server answers questions about a particular LilyPond version — what commands exist, what the initialisation files in `share/lilypond/<version>/ly` define, what the word list under `vim/syntax` contains. Those answers change from version to version, so the tests read them from a real installation rather than from copies checked into this repository. One consequence is worth stating plainly: **a test run with no LilyPond installed fails**, rather than quietly skipping the tests that need one and reporting success.

`tests/common/mod.rs` does the finding. `require_installs()` returns every installation it can see, oldest first, at most one per version, and a test that depends on a version loops over them:

```rust
mod common;
use common::require_installs;

#[test]
fn something_about_the_installation() {
    for install in require_installs() {
        // install.share_dir(), install.words_file(), install.executable()
    }
}
```

So on a development machine the tests run against every version you have installed, and in CI against exactly the version that job installed. Put the version in any assertion message: it's the difference between "this is broken" and "this is broken on 2.24.1".

An installation is recognised by its layout: a `bin/lilypond` (or `bin/lilypond.exe`) next to a `share/lilypond/<version>` directory. The version comes from that directory's name rather than from running the executable, so an installation unpacked for another architecture is still usable for the many tests that only read files from it.

### Where installations are looked for

With `LILYPOND_TEST_INSTALL_DIR` set, only that directory is searched; a relative value is resolved against the working directory, which `cargo test` sets to the package root.

Otherwise the search covers your home directory, the parent of any `lilypond` on `PATH`, and the usual places for the platform: `C:\Program Files` and `C:\Program Files (x86)` on Windows, `/Applications`, `/opt` and `/usr/local` on macOS, `/opt`, `/usr/local` and `/usr` elsewhere. It descends two levels, but only into directories with "lilypond" in the name, so the Windows installer's habit of grouping versions (`C:\Program Files (x86)\LilyPond\lilypond-2.24.3`) is covered without walking the whole of Program Files.

`tests/lilypond_install.rs` checks that whatever is found has the files the server reads. When it fails, the problem is the harness or the installation, not the server.

### Getting another version to test against

`scripts/install-lilypond.sh <version>` downloads an official binary release and unpacks it, printing the installation root:

```bash
scripts/install-lilypond.sh 2.24.3               # unpacks into ~/lilypond-installs
scripts/install-lilypond.sh 2.24.3 /opt/lilypond # or wherever you like
```

The default destination, `~/lilypond-installs`, is inside the search path, so a version installed this way is picked up on the next run with nothing else to configure. Unpacking several versions into one directory is the intended way to test against all of them at once. Re-running for a version that's already there does nothing, so it's cheap to call from a script.

The script knows the layout of the 2.24 and later binary releases, which are published as generic packages on the LilyPond GitLab release. Earlier series were distributed differently and will need a new branch in the script when we come to support them.

## CI

The `build` job's matrix is the supported operating systems crossed with the supported LilyPond versions, so each combination gets its own run and a failure names the version it happened on. Each job installs its version with the same script into `.lilypond-installs` in the workspace and points `LILYPOND_TEST_INSTALL_DIR` at it, which pins the run to that one version.

To start testing against another version, add it to the `lilypond` list in `.github/workflows/ci.yml`. Nothing else needs to change.

The `lint` job — `cargo fmt --check` and `cargo clippy --all-targets` — needs no installation, since it compiles the tests without running them.

The VS Code extension that ships this server uses the same approach for its own tests; see `TESTING.md` in that repository. The two implementations are independent on purpose, so this repository stands alone, but they share the environment variable name and the installer script, and a change to one is usually worth making in the other.

## The file-based suites

The refactorings have their own test formats, so that a case is a piece of LilyPond source and its expected result rather than a wall of Rust:

- **Extract to variable**, in `tests/extract/`. Each `.extract` file holds one or more cases: annotated source with a `^` underline marking the selection, in the style of vscode-tmgrammar-test, followed by the expected document. See [`tests/extract/FORMAT.md`](tests/extract/FORMAT.md).
- **Inline variable**, the inverse, in `tests/inline/`, where a `^here`/`^all` caret marks the cursor and the action to run. See [`tests/inline/FORMAT.md`](tests/inline/FORMAT.md). On top of that, `tests/extract.rs` drives every extract case through extract *then* inline and checks the resolved music is unchanged, exercising the two as inverses.
- **Make explicit** (durations, pitches, and both), in `tests/explicit/`, where each case pairs one selection with the output of all three actions (`--- dur`/`--- pitch`/`--- both`). See [`tests/explicit/FORMAT.md`](tests/explicit/FORMAT.md).

Each `FORMAT.md` covers the syntax, how to write a new case, and how to read a failure.

## Writing tests

- Test designed, expected behaviour, not temporary behaviour or accepted limitations. A test that will start failing once a bug is fixed is worse than no test.
- Prefer a case in one of the file-based suites over a new Rust test when the behaviour is a source-to-source transformation.
- Reach for a real installation over a fixture whenever the behaviour under test depends on what LilyPond ships.
