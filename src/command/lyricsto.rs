//! `\lyricsto [voice] music`.
//!
//! A reserved word, like `\new`/`\context` ([`new_context`](super::new_context))
//! and `\change` ([`change`](super::change)) — but grammatically the plainest
//! of the four: `voice` is an ordinary optional argument no `named_context` or
//! `assignment_lhs` wrapping ever touches, so [`default_parse`](super::default_parse)
//! walks [`LYRICSTO_PARAMS`] with no override needed. The only thing that
//! makes this its own file rather than a plain [`Row`](super::Row) is
//! [`completions`](Command::completions): `voice` names the same
//! context-instance namespace `\change` and `\new`/`\context`'s `= "name"`
//! do, and those candidates come from the document's [`Scope`], not from a
//! fixed table — see [`context_instance_candidates`](super::context_instance_candidates).

use super::static_command::{StaticCommand, curated, static_command};
use super::{
    ArgKind, Candidate, Command, CommandCall, CompletionContext, Documentation, MusicContext,
    NoteEntry, Param, context_instance_candidates,
};
use crate::vocabulary::Scope;

/// `voice` and `music` — kept here rather than in [`super`] now that
/// `\lyricsto` is the only command that needs this exact shape.
static LYRICSTO_PARAMS: &[Param] = &[
    Param::optional("voice", ArgKind::ContextName),
    Param::required("music", ArgKind::Music),
];

const LYRICSTO_DOC: &str = "Aligns the lyrics in `music` under the notes of `voice` — the \
     instance name a `\\new`/`\\context` elsewhere gave a `Voice`, e.g. `\\lyricsto \"vocals\" \
     { … }`. Without `voice`, aligns under the nearest preceding `Voice` in the same \
     simultaneous music instead.";

/// `\lyricsto`. Wraps a [`StaticCommand`] for everything but
/// [`completions`](Command::completions), whose `voice` candidates
/// ([`context_instance_candidates`](super::context_instance_candidates))
/// depend on the document's [`Scope`] and so can't live in the static
/// completions table [`StaticCommand`] otherwise reads.
pub(super) struct LyricstoCommand {
    base: StaticCommand,
}

impl Command for LyricstoCommand {
    fn name(&self) -> &str {
        self.base.name()
    }

    fn signature(&self) -> &[Param] {
        self.base.signature()
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.base.documentation()
    }

    // `StaticCommand::music_context` reads its own stored `context` (fixed
    // here at `NoteEntry::NonNote`, set below); the trait's default
    // instead always returns `ambient` unchanged, which is right for
    // `\change`'s `Inherit` (indistinguishable from the default either way)
    // but would silently drop `\lyricsto`'s always-`NonNote` body if left
    // unforwarded here, exactly as an earlier draft of this file did — a
    // half-typed `\lyricsto v { you were found }` then misreads its lyric
    // words as notes. Forwarded explicitly so this bespoke wrapper can never
    // regress that quietly again.
    fn music_context(
        &self,
        call: &CommandCall,
        ambient: MusicContext,
        scope: &Scope,
    ) -> MusicContext {
        self.base.music_context(call, ambient, scope)
    }

    fn completions(&self, index: usize, ctx: &CompletionContext) -> Vec<Candidate> {
        match index {
            0 => context_instance_candidates(ctx.scope),
            _ => self.base.completions(index, ctx),
        }
    }
}

/// Builds the `\lyricsto` entry for [`RESERVED`](super::RESERVED).
pub(super) fn command() -> LyricstoCommand {
    LyricstoCommand {
        base: static_command(
            "lyricsto",
            LYRICSTO_PARAMS,
            NoteEntry::NonNote,
            curated(LYRICSTO_DOC),
            &[],
        ),
    }
}

#[cfg(test)]
mod tests {
    // The quoted-voice-name and no-voice-name shapes already have coverage
    // in `crate::command`'s own test module (`lyricsto_reads_an_optional_voice_name_then_its_body`,
    // `lyricsto_with_no_voice_name_still_reads_its_body`), left there rather
    // than duplicated here. What's added below is what wasn't covered
    // anywhere: the bare-symbol voice name, and the `music_context` forward
    // this file exists to get right.
    use crate::command::{self, Arg, MusicContext, NoteEntry};
    use crate::note_names::fixture_language;
    use crate::vocabulary::Scope;
    use tree_sitter::{Node, Tree};

    fn tree(src: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_lilypond::LANGUAGE_LILYPOND.into())
            .expect("load grammar");
        parser.parse(src, None).expect("parse")
    }

    fn call(src: &str) -> Option<command::CommandCall> {
        let tree = tree(src);
        let root = tree.root_node();
        let mut cursor = root.walk();
        let children: Vec<Node> = root.children(&mut cursor).collect();
        let start = children.iter().position(|n| n.kind() == "escaped_word")?;
        let scope = Scope::builtins_only();
        command::parse(&children, start, src, &fixture_language(), &scope).map(|(call, _)| call)
    }

    #[test]
    fn reads_a_bare_symbol_voice_name_too() {
        let call = call("\\lyricsto vocals { la }").expect("a lyricsto call");
        assert!(matches!(&call.args[0], Arg::ContextName { name, .. } if name == "vocals"));
    }

    #[test]
    fn music_context_is_non_note_not_the_trait_default() {
        // The regression this guards: an earlier draft left `music_context`
        // unforwarded, so it fell back to the trait default (return
        // `ambient` unchanged) instead of `StaticCommand`'s stored
        // `NonNote`, and lyric words were misread as notes.
        let call = call("\\lyricsto \"v\" { la }").expect("a lyricsto call");
        let context = call.cmd.music_context(
            &call,
            MusicContext::new(NoteEntry::Absolute, fixture_language()),
            &Scope::builtins_only(),
        );
        assert_eq!(context.entry, NoteEntry::NonNote);
    }
}
