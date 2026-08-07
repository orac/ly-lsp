//! Checks that the tests can find a LilyPond installation and that it has the shape the server reads files out of.
//!
//! Everything version-specific the server learns comes from these directories, so if this test fails, the failures in any test that reads an installation would be about the harness, not the server.

mod common;

use common::require_installs;

#[test]
fn every_installation_has_the_files_the_server_reads() {
    for install in require_installs() {
        assert!(
            install.executable().is_file(),
            "{} has no lilypond executable",
            install.root.display()
        );
        assert!(
            install.words_file().is_file(),
            "LilyPond {} has no word list at {}",
            install.version,
            install.words_file().display()
        );
        assert!(
            install.share_dir().join("ly").is_dir(),
            "LilyPond {} has no initialisation files at {}",
            install.version,
            install.share_dir().join("ly").display()
        );
    }
}

#[test]
fn the_word_list_names_commands_with_a_backslash() {
    for install in require_installs() {
        let words = std::fs::read_to_string(install.words_file()).unwrap();

        // The file escapes the leading backslash of a command, so a reader has to un-escape before matching against source.
        assert!(
            words.lines().any(|line| line == r"\\relative"),
            "LilyPond {} does not list \\relative",
            install.version
        );
    }
}
