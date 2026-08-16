//! `\change type = "name"`.
//!
//! Moves the music that follows into an *already-existing* context of
//! `type` named `name`, rather than creating one — see the reasoning on
//! [`context::read_instances`](crate::context::read_instances) for why that
//! makes `\change` a reference to a context instance name, not a second
//! source of definitions for the namespace [`context`](crate::context)
//! builds.
//!
//! A reserved word, like `\new`/`\context`
//! ([`new_context`](super::new_context)) — LilyPond's grammar recognises it
//! before any name lookup happens, so nothing in a file can shadow it — but
//! its grammar shape is neither `new_context`'s `named_context` node nor a
//! plain row [`default_parse`](super::default_parse) could walk cleanly.
//! `\change Staff` on its own leaves `Staff` a bare `symbol`, exactly what
//! [`ArgKind::ContextType`](super::ArgKind::ContextType) already reads — but
//! the moment a `= "name"` follows, LilyPond parses the whole thing as
//! though it were spelling out an assignment, the same way `\set
//! Staff.instrumentName = "…"` does (see
//! [`consume_property_path`](super::consume_property_path)): the context
//! type ends up wrapped in an `assignment_lhs` node, which
//! `ArgKind::ContextType`'s ordinary matcher doesn't accept. So this gets a
//! bespoke [`parse_args`](Command::parse_args) that tries the plain shape
//! first and falls back to unwrapping the `assignment_lhs`, rather than
//! reusing `default_parse` outright.

use super::static_command::{StaticCommand, curated, static_command};
use super::{
    Arg, ArgKind, ArgReader, Candidate, Command, CompletionContext, Documentation, MusicContext,
    Param, context_instance_candidates, context_type_candidates,
};
use crate::line_struct::Span;

/// `type` and `name` — the two pieces [`ChangeCommand::parse_args`] reads by
/// hand, listed here only so signature help and arity checks have parameter
/// names to show; see the module doc for why `default_parse` can't walk this
/// signature on its own.
static CHANGE_PARAMS: &[Param] = &[
    Param::required("type", ArgKind::ContextType),
    Param::required("name", ArgKind::ContextName),
];

/// `\change`. Wraps a [`StaticCommand`] for its
/// `name`/`signature`/`documentation`/`completions`, overriding only
/// [`parse_args`](Command::parse_args) for the `assignment_lhs`-wrapped
/// context type the grammar hands it.
pub(super) struct ChangeCommand {
    base: StaticCommand,
}

impl Command for ChangeCommand {
    fn name(&self) -> &str {
        self.base.name()
    }

    fn signature(&self) -> &[Param] {
        self.base.signature()
    }

    fn parse_args(&self, args: &mut ArgReader) -> Vec<Arg> {
        let mut out = Vec::new();

        // The context type is a bare `symbol` when nothing follows it
        // (`\change Staff`, still being typed) — exactly what
        // `ArgKind::ContextType` already reads, tried first here. Only once
        // a `= "name"` is present does the grammar wrap it in an
        // `assignment_lhs` instead, which `take` can't unwrap on its own: it
        // has nothing else inside, so the wrapper's own span and text are
        // exactly the type's, and `skip_one` steps over it as a single unit
        // once `peek`/`peek_text` have read it.
        let context_type = args.take(&ArgKind::ContextType).or_else(|| {
            let node = args.peek()?;
            if node.kind() != "assignment_lhs" {
                return None;
            }
            let text = args.peek_text()?;
            let arg = Arg::ContextType {
                span: Span::new(node.start_byte(), node.end_byte()),
                name: text.to_string(),
            };
            args.skip_one();
            Some(arg)
        });
        let Some(context_type) = context_type else {
            return out;
        };
        out.push(context_type);

        if args.take_punct("=")
            && let Some(name) = args.take(&ArgKind::ContextName)
        {
            out.push(name);
        }
        out
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.base.documentation()
    }

    fn completions(&self, index: usize, ctx: &CompletionContext) -> Vec<Candidate> {
        match index {
            0 => context_type_candidates(ctx.scope),
            1 => context_instance_candidates(ctx.scope),
            _ => self.base.completions(index, ctx),
        }
    }
}

const CHANGE_DOC: &str = "Moves the music that follows into an already-existing context of \
     `type` named `name` — unlike `\\new`, which always creates a fresh context, or `\\context`, \
     which creates one only if none is found. `name` refers back to an instance a `\\new` or \
     `\\context` elsewhere named.";

/// Builds the `\change` entry for [`RESERVED`](super::RESERVED).
pub(super) fn command() -> ChangeCommand {
    ChangeCommand {
        base: static_command(
            "change",
            CHANGE_PARAMS,
            MusicContext::Inherit,
            curated(CHANGE_DOC),
            &[],
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::command::{self, Arg};
    use crate::note_names::Language;
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
        let start = children
            .iter()
            .position(|n| n.kind() == "escaped_word" || n.kind() == "named_context")?;
        let scope = Scope::builtins_only();
        command::parse(&children, start, src, Language::DEFAULT, &scope).map(|(call, _)| call)
    }

    #[test]
    fn reads_the_type_and_the_quoted_name() {
        let call = call("\\change Staff = \"lower\"").expect("a change call");
        assert_eq!(call.name, "change");
        assert!(matches!(&call.args[0], Arg::ContextType { name, .. } if name == "Staff"));
        assert!(matches!(&call.args[1], Arg::ContextName { name, .. } if name == "lower"));
        assert_eq!(call.args.len(), 2);
    }

    #[test]
    fn reads_a_bare_symbol_name_too() {
        let call = call("\\change Staff = lower").expect("a change call");
        assert!(matches!(&call.args[1], Arg::ContextName { name, .. } if name == "lower"));
    }

    #[test]
    fn a_half_typed_change_with_no_name_yet_still_reads_the_type() {
        let call = call("\\change Staff").expect("a change call, however incomplete");
        assert!(matches!(&call.args[0], Arg::ContextType { name, .. } if name == "Staff"));
        assert_eq!(call.args.len(), 1);
    }
}
