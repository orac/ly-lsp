//! What one file defines, as a [`Layer`] of commands.
//!
//! `foo = { c d e }` binds a name that `\foo` substitutes, taking no arguments:
//! anything written after the call belongs to the enclosing music, not to the
//! call. So a definition *is* a command — a zero-argument one unless something
//! says otherwise — and the split between "definitions" (a name and a place)
//! and "commands" (a name and a signature) collapses. One layer per file holds
//! both, so the stacking, shadowing and invalidation machinery serves both, and
//! "where is `\foo` defined?" and "what does `\foo` do?" are one lookup rather
//! than two.
//!
//! Both readers — [`SYMBOL_QUERY`](crate::document::SYMBOL_QUERY) for the
//! LilyPond assignment, [`scheme`](super::scheme) for what `#( … )` binds —
//! produce [`Binding`]s, which [`layer`] turns into that table. A binding whose
//! value says nothing about its arguments becomes a
//! [`Variable`](super::variable::Variable).

use std::collections::HashMap;
use std::sync::Arc;

use crate::line_struct::Span;
use crate::vocabulary::Layer;

use super::variable::Variable;
use super::{
    Arg, ArgReader, Candidate, CheckContext, Command, CommandCall, CompletionContext,
    Documentation, MusicContext, Param,
};

/// One name a file binds, as either reader found it — the input [`layer`]
/// builds a file's table from.
pub struct Binding {
    pub name: String,
    /// The name as written — what go-to-definition navigates to and rename
    /// rewrites. Not the whole definition: a `\foo` reference is replaced by
    /// `\newFoo`, and the name on the left of `foo = { … }` by `newFoo`.
    pub span: Span,
    /// Where the value is written, for a [`Variable`] to summarise on hover.
    /// `None` where the reader can't point at one — a Scheme binding form, or
    /// an assignment with nothing after its `=`.
    pub value: Option<Span>,
    /// What `\name` calls, where the definition says what arguments it takes.
    /// `None` for a name bound to a plain value, which becomes a [`Variable`].
    pub command: Option<Arc<dyn Command>>,
}

impl Binding {
    /// A name bound to something with no signature of its own, written at
    /// `value` where the reader could tell.
    pub fn variable(name: impl Into<String>, span: Span, value: Option<Span>) -> Self {
        Self {
            name: name.into(),
            span,
            value,
            command: None,
        }
    }
}

/// Builds a file's [`Layer`] from every name it binds.
///
/// `bindings` must be in source order: a name bound twice keeps the *last*
/// binding, which is the one LilyPond itself resolves to, with the one it
/// replaced hanging off it (see [`Command::redefines`]).
///
/// `source` is the text those bindings were read from — a layer belongs to one
/// file, so there is exactly one. Shared, not copied: every [`Variable`] holds
/// it, and reads its own value out of it only if hover ever asks.
///
/// `origin` is what to call that file when hover says where a command came
/// from — see [`Layer::origin`].
pub fn layer(
    bindings: impl IntoIterator<Item = Binding>,
    source: Arc<str>,
    origin: Arc<str>,
) -> Layer {
    let mut commands: HashMap<String, Arc<dyn Command>> = HashMap::new();
    for Binding {
        name,
        span,
        value,
        command,
    } in bindings
    {
        let command = command.unwrap_or_else(|| match value {
            Some(value) => Arc::new(Variable::bound_to(name.clone(), Arc::clone(&source), value)),
            None => Arc::new(Variable::new(name.clone())),
        });
        let redefines = commands.remove(&name);
        commands.insert(
            name,
            Arc::new(Definition {
                command,
                span,
                redefines,
            }),
        );
    }
    Layer::new(origin, commands)
}

/// A command as one file defines it: what `\name` does, where the name was
/// written, and — where the file binds the same name more than once — the
/// definition this one replaced.
///
/// A decorator rather than a pair of fields on each [`Command`] impl, so that
/// "where it came from" is written once and works for every source of
/// knowledge: a [`Variable`], a [`SchemeCommand`](super::scheme::SchemeCommand),
/// and whatever the install layer turns out to need. Everything but
/// [`definition`](Command::definition) and [`redefines`](Command::redefines) is
/// the decorated command's to answer.
struct Definition {
    command: Arc<dyn Command>,
    span: Span,
    redefines: Option<Arc<dyn Command>>,
}

impl Command for Definition {
    fn definition(&self) -> Option<Span> {
        Some(self.span)
    }

    fn redefines(&self) -> Option<&Arc<dyn Command>> {
        self.redefines.as_ref()
    }

    fn name(&self) -> &str {
        self.command.name()
    }

    fn signature(&self) -> &[Param] {
        self.command.signature()
    }

    fn parse_args(&self, args: &mut ArgReader) -> Vec<Arg> {
        self.command.parse_args(args)
    }

    fn music_context(&self, call: &CommandCall, ambient: MusicContext) -> MusicContext {
        self.command.music_context(call, ambient)
    }

    fn synopsis(&self) -> Option<String> {
        self.command.synopsis()
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.command.documentation()
    }

    fn completions(&self, index: usize, ctx: &CompletionContext) -> Vec<Candidate> {
        self.command.completions(index, ctx)
    }

    fn check(
        &self,
        call: &CommandCall,
        ctx: &CheckContext,
    ) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        self.command.check(call, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::definition_spans;

    fn binding(name: &str, at: usize) -> Binding {
        Binding::variable(name, Span::new(at, at + name.len()), None)
    }

    /// A layer of bindings that point at no source, which suits every test
    /// here: they ask what a layer resolves to, never what a value looks like.
    fn layer(bindings: impl IntoIterator<Item = Binding>) -> Layer {
        super::layer(bindings, Arc::from(""), Arc::from("test.ly"))
    }

    #[test]
    fn a_binding_with_no_signature_is_a_command_that_takes_no_arguments() {
        let layer = layer([binding("foo", 0)]);
        let foo = layer.get("foo").expect("foo");
        assert_eq!(foo.name(), "foo");
        assert!(foo.signature().is_empty());
    }

    #[test]
    fn a_name_bound_twice_keeps_both_definitions_in_source_order() {
        // Only the later binding answers `\foo`, but both places it was bound
        // are reachable from it — go-to-definition offers both today, and a
        // "this replaces an earlier definition" warning would need the chain.
        let layer = layer([binding("foo", 0), binding("foo", 20)]);
        let foo = layer.get("foo").expect("foo");
        assert_eq!(
            definition_spans(foo.as_ref()),
            vec![Span::new(0, 3), Span::new(20, 23)]
        );
    }

    #[test]
    fn distinct_names_do_not_chain() {
        let layer = layer([binding("foo", 0), binding("bar", 20)]);
        assert_eq!(layer.len(), 2);
        for name in ["foo", "bar"] {
            let command = layer.get(name).expect(name);
            assert_eq!(definition_spans(command.as_ref()).len(), 1);
            assert!(command.redefines().is_none());
        }
    }

    #[test]
    fn a_builtin_command_is_defined_nowhere() {
        // The hand-written layer's commands are defined in this repo, not in
        // anyone's score, so there is nothing to navigate to.
        let repeat = crate::command::RESERVED.get("repeat").expect("repeat");
        assert!(definition_spans(repeat.as_ref()).is_empty());
    }
}
