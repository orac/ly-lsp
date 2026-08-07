//! Finding the LilyPond installations that the tests run against.
//!
//! The server reads files out of a LilyPond installation — the initialisation files under `share/lilypond/<version>/ly`, the word list under `vim/syntax`, and so on — so the tests need a real installation rather than a copy of a few files checked into the repository. That way the same tests run against every version we claim to support. See `TESTING.md` for how to get one.

// Every integration test binary compiles this module separately, so helpers that one test file doesn't happen to call would be reported as dead there.
#![allow(dead_code)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Name of the environment variable that, when set, replaces the search of well-known install locations.
pub const INSTALL_DIR_VARIABLE: &str = "LILYPOND_TEST_INSTALL_DIR";

/// How deep below a search location an installation may be nested. The extra level covers installers that group their versions in a shared parent, as the Windows installer does with `C:\Program Files (x86)\LilyPond\lilypond-2.24.3`.
const SEARCH_DEPTH: usize = 2;

const EXECUTABLE_NAME: &str = if cfg!(windows) {
    "lilypond.exe"
} else {
    "lilypond"
};

/// A LilyPond installation found on the machine running the tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LilyPondInstall {
    /// The directory containing `bin/` and `share/`.
    pub root: PathBuf,
    /// Three-part version, read from the directory name under `share/lilypond`.
    pub version: String,
}

impl LilyPondInstall {
    /// The `lilypond` executable. Present on disk, but not necessarily runnable: a CI runner may hold an installation built for another architecture.
    pub fn executable(&self) -> PathBuf {
        self.root.join("bin").join(EXECUTABLE_NAME)
    }

    /// The directory of version-specific data files: `ly/`, `scm/`, `fonts/` and friends.
    pub fn share_dir(&self) -> PathBuf {
        self.root.join("share").join("lilypond").join(&self.version)
    }

    /// The file listing every command and keyword this version knows.
    pub fn words_file(&self) -> PathBuf {
        self.share_dir()
            .join("vim")
            .join("syntax")
            .join("lilypond-words")
    }

    fn version_key(&self) -> (u32, u32, u32) {
        let mut parts = self
            .version
            .split('.')
            .map(|part| part.parse().unwrap_or(0));
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    }
}

/// Finds every LilyPond installation the tests can run against, oldest first.
///
/// With `LILYPOND_TEST_INSTALL_DIR` set, only that directory is searched — this is how CI pins the run to the version it installed. Otherwise the well-known install locations for the platform are searched, along with anything reachable through `PATH`, so a developer's machine tests against everything they have installed.
///
/// At most one installation is returned per version, since testing the same version twice tells us nothing new.
pub fn discover_installs() -> Vec<LilyPondInstall> {
    let mut found: Vec<LilyPondInstall> = Vec::new();
    for location in search_locations() {
        for install in installs_under(&location, SEARCH_DEPTH) {
            if !found.iter().any(|other| other.version == install.version) {
                found.push(install);
            }
        }
    }

    found.sort_by_key(|install| install.version_key());
    found
}

/// Finds the installations to test against, and explains how to get one if there are none.
///
/// # Panics
///
/// If no installation can be found: a test run that silently skipped every version-dependent test would report success without having checked anything.
pub fn require_installs() -> Vec<LilyPondInstall> {
    let installs = discover_installs();
    assert!(
        !installs.is_empty(),
        "No LilyPond installation found, so the tests that need one cannot run. Install LilyPond from https://lilypond.org/download.html, or run scripts/install-lilypond.sh <version>, then set {INSTALL_DIR_VARIABLE} to the directory holding the installation. See TESTING.md."
    );
    installs
}

fn search_locations() -> Vec<PathBuf> {
    if let Some(configured) = env::var_os(INSTALL_DIR_VARIABLE) {
        return vec![PathBuf::from(configured)];
    }

    // The home directory covers the installer script's default destination, ~/lilypond-installs, as well as installations unpacked by hand.
    let mut locations: Vec<PathBuf> = home_dir().into_iter().collect();
    locations
        .extend(executable_dirs_on_path().filter_map(|dir| dir.parent().map(Path::to_path_buf)));

    if cfg!(windows) {
        locations.push(PathBuf::from(r"C:\Program Files"));
        locations.push(PathBuf::from(r"C:\Program Files (x86)"));
        locations.extend(env::var_os("LOCALAPPDATA").map(PathBuf::from));
    } else if cfg!(target_os = "macos") {
        locations.extend(["/Applications", "/opt", "/usr/local"].map(PathBuf::from));
    } else {
        locations.extend(["/opt", "/usr/local", "/usr"].map(PathBuf::from));
    }

    locations
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The `bin` directories of any `lilypond` executables on `PATH`.
fn executable_dirs_on_path() -> impl Iterator<Item = PathBuf> {
    let path = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path)
        .filter(|entry| entry.join(EXECUTABLE_NAME).is_file())
        .collect::<Vec<_>>()
        .into_iter()
}

/// Collects the installations at or below a directory.
///
/// Only subdirectories named after LilyPond are descended into: the search locations include directories with hundreds of unrelated children, and walking those would cost far more than it could ever find.
fn installs_under(directory: &Path, depth: usize) -> Vec<LilyPondInstall> {
    let installs = read_installs(directory);
    if !installs.is_empty() || depth == 0 {
        return installs;
    }

    child_directories(directory)
        .filter(|child| {
            child
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.to_lowercase().contains("lilypond"))
        })
        .flat_map(|child| installs_under(&child, depth - 1))
        .collect()
}

/// Recognises installations by the layout LilyPond's own packages use, and returns nothing for any other directory.
///
/// The versions are the names of the directories under `share/lilypond`. Reading them beats running the executable, which may have been unpacked for another architecture. A distribution package can offer several versions from one root, so this returns a list.
fn read_installs(root: &Path) -> Vec<LilyPondInstall> {
    if !root.join("bin").join(EXECUTABLE_NAME).is_file() {
        return Vec::new();
    }

    child_directories(&root.join("share").join("lilypond"))
        .filter_map(|version_dir| {
            let version = version_dir.file_name()?.to_str()?.to_owned();
            looks_like_version(&version).then(|| LilyPondInstall {
                root: root.to_path_buf(),
                version,
            })
        })
        .collect()
}

fn looks_like_version(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn child_directories(directory: &Path) -> impl Iterator<Item = PathBuf> {
    // Search locations that don't exist, and ones the test runner may not read, are simply not places an installation was found.
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
}
