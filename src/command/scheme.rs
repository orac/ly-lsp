//! Reading definitions out of a file's embedded Scheme.
//!
//! `myFunc = #(define-music-function (music) (ly:music?) …)` is the way a user
//! adds a command of their own, and the only way the server can learn about
//! one. This module recognises the definition *forms* — `define-music-function`
//! and its `event`/`scheme`/`void` siblings — and turns each into a
//! [`SchemeCommand`], a [`Command`] like any other.
//!
//! It reports every name a file's Scheme binds, as a
//! [`Binding`], whether or not the value says what
//! arguments it takes: `#(define-public foo 42)` has no signature to read, but
//! LilyPond will still substitute `\foo` for it, so the name is defined and
//! must not be flagged as an undefined reference. Those bindings have no
//! `assignment_lhs` for the symbol query to match on, so this is the only
//! reader that sees them at all.
//!
//! # Read, don't evaluate
//!
//! Recognising `(define-music-function (a b) (pred? pred?) "doc" …)` is a
//! datum-shape match, not a computation. Reading avoids embedding a Scheme
//! interpreter, and avoids executing workspace-authored code in the server
//! process. We never look at a function's *body*: `#{ … #}` makes the LilyPond
//! and Scheme readers mutually recursive, and signatures alone dodge that
//! entirely.
//!
//! The design notes in [`doc/command-parsing.md`](../../doc/command-parsing.md)
//! called for `tree-sitter-scheme` to do the reading. It turned out not to be
//! needed: the LilyPond grammar already parses `#( … )` into `scheme_list` /
//! `scheme_symbol` / `scheme_string` nodes, with the same error recovery and
//! incremental reparse the rest of the document gets, so this module reads the
//! tree we already have rather than adding a second grammar.
//!
//! # What is deliberately approximate
//!
//! A predicate is not a *shape*: `(number? ly:music?)` says what a value must
//! satisfy once evaluated, not how it is written. [`arg_kind`] maps the handful
//! of predicates whose source shape we actually know, and everything else
//! becomes an [`ArgKind::Unknown`] that consumes exactly one node — right often
//! enough to beat refusing the whole signature, and the reason one unrecognised
//! predicate can't void the rest of a function's arguments.

use std::borrow::Cow;
use std::sync::Arc;

use tree_sitter::{Node, Tree};

use super::definition::Binding;
use super::{ArgKind, Command, DocSource, Documentation, Param};
use crate::line_struct::Span;

/// The definition forms that produce a `\command`. `define-void-function` and
/// `define-scheme-function` are here alongside the two music ones because all
/// four are called the same way in LilyPond source, which is all a signature
/// cares about.
const FUNCTION_FORMS: &[&str] = &[
    "define-music-function",
    "define-event-function",
    "define-scheme-function",
    "define-void-function",
];

/// The binding forms that give a function a name from inside Scheme, rather
/// than through a LilyPond assignment. `#(define-public myFunc (define-music-function …))`
/// produces no `assignment_lhs`, so this is the only way that shape is visible
/// at all.
const BINDING_FORMS: &[&str] = &["define", "define-public", "define*", "define*-public"];

/// A command read from a `define-…-function` in a user's file.
///
/// One struct for the whole layer, instantiated once per definition — the
/// "one impl per source of knowledge" [`Command`] describes. Everything it
/// knows is owned: unlike the hand-written commands, its name, parameters and
/// documentation all come from source that can change under it.
pub struct SchemeCommand {
    name: String,
    params: Vec<Param>,
    documentation: Option<Documentation>,
}

impl Command for SchemeCommand {
    fn name(&self) -> &str {
        &self.name
    }

    fn signature(&self) -> &[Param] {
        &self.params
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.documentation.as_ref()
    }

    // No `music_context` override: a music function's body arguments are read
    // in whatever context the call itself was found in, which is what the
    // trait's default (returning `ambient`) already says. Only the mode-switch
    // keywords establish a context of their own, and those can't be defined
    // this way.
}

/// Reads every name `tree`'s embedded Scheme binds, in source order.
///
/// A binding whose value is one of the [`FUNCTION_FORMS`] carries a
/// [`SchemeCommand`] saying what arguments `\name` takes; every other binding
/// carries none, and becomes a plain
/// [`Variable`](super::definition::Variable) — a signature would be an
/// invention, but the name is defined either way, since LilyPond substitutes
/// `\foo` for anything bound in the module.
///
/// Called once per [`Document`](crate::document::Document), so a file included
/// from a dozen scores is read once rather than a dozen times: the resulting
/// layer is shared by every [`Scope`](crate::vocabulary::Scope) that reaches
/// the file.
pub fn read(tree: &Tree, src: &str) -> Vec<Binding> {
    let mut out = Vec::new();
    collect(tree.root_node(), src, &mut out);
    out
}

/// Walks `node` for `embedded_scheme` blocks, reading each for a definition.
/// Doesn't descend into a block once found: a definition nested in a function's
/// *body* is out of scope by design (see the module docs).
fn collect(node: Node, src: &str, out: &mut Vec<Binding>) {
    if node.kind() == "embedded_scheme" {
        read_embedded(node, src, out);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, src, out);
    }
}

/// Reads one `#( … )` block for a definition, naming it from the assignment it
/// is the value of (`myFunc = #( … )`) where there is one.
fn read_embedded(node: Node, src: &str, out: &mut Vec<Binding>) {
    let Some(list) = child_of_kind(node, "embedded_scheme_text")
        .and_then(|text| child_of_kind(text, "scheme_list"))
    else {
        return;
    };
    read_definition(list, assigned_name(node, src), src, out);
}

/// The `symbol` node an `embedded_scheme` node is assigned to: `myFunc` in
/// `myFunc = #( … )`. The grammar leaves the three as flat siblings, so this
/// walks back over the `=` to the `assignment_lhs` — the same node the symbol
/// query captures as a definition, which is what keeps this view of a
/// definition and go-to-definition's view of it in step.
fn assigned_name<'a>(node: Node<'a>, src: &str) -> Option<Node<'a>> {
    let equals = node.prev_sibling()?;
    if text(equals, src) != "=" {
        return None;
    }
    let lhs = equals.prev_sibling()?;
    if lhs.kind() != "assignment_lhs" {
        return None;
    }
    child_of_kind(lhs, "symbol")
}

/// Reads a Scheme list that may be a definition, named either by the LilyPond
/// assignment it is the value of or by the binding form it sits in.
fn read_definition(list: Node, assigned: Option<Node>, src: &str, out: &mut Vec<Binding>) {
    let elements = items(list);
    let Some(head) = elements.first().filter(|n| n.kind() == "scheme_symbol") else {
        return;
    };
    let head = text(*head, src);

    if FUNCTION_FORMS.contains(&head) {
        // Only a name makes a function reachable as `\foo`; an anonymous one
        // in value position is nothing we can offer. The name's span is the
        // assignment's left-hand side — the very node
        // [`SYMBOL_QUERY`](crate::document::SYMBOL_QUERY) captures, which is
        // how the document tells this binding and the query's view of it apart
        // and keeps only one.
        if let Some(name) = assigned
            && let Some(command) = function(&elements, text(name, src), src)
        {
            out.push(binding(name, src, Some(Arc::new(command))));
        }
        return;
    }

    if BINDING_FORMS.contains(&head)
        && let [_, name, rest @ ..] = elements.as_slice()
        && name.kind() == "scheme_symbol"
    {
        // `#(define-public myFunc …)`. The binding defines `myFunc` whatever
        // its value, because LilyPond resolves `\myFunc` against the module the
        // binding lands in. Whether it also has a *signature* depends on the
        // value being one of the definition forms — matched on the value rather
        // than on `define-public` itself, so `#(define-public answer 42)`
        // doesn't fabricate arguments for something that has none. A
        // procedure's name, `(define (f x) …)`, is a list rather than a symbol
        // and so isn't a definition at all: `\f` wouldn't reach it either.
        let command = rest
            .first()
            .filter(|n| n.kind() == "scheme_list")
            .and_then(|value| defined_function(*value, text(*name, src), src));
        out.push(binding(*name, src, command));
    }
}

/// The command a binding's value defines, if that value is one of the
/// [`FUNCTION_FORMS`] rather than an ordinary Scheme expression.
fn defined_function(list: Node, name: &str, src: &str) -> Option<Arc<dyn Command>> {
    let elements = items(list);
    let head = elements.first().filter(|n| n.kind() == "scheme_symbol")?;
    if !FUNCTION_FORMS.contains(&text(*head, src)) {
        return None;
    }
    Some(Arc::new(function(&elements, name, src)?))
}

/// The [`Binding`] a name node makes, of `command` where the definition says
/// what arguments it takes.
fn binding(node: Node, src: &str, command: Option<Arc<dyn Command>>) -> Binding {
    Binding {
        name: text(node, src).to_string(),
        span: Span::new(node.start_byte(), node.end_byte()),
        command,
    }
}

/// Builds a [`SchemeCommand`] from the elements of a `(define-…-function …)`
/// list. `None` when the list doesn't have the argument and predicate lists a
/// definition needs, i.e. when it is still half-typed.
fn function(elements: &[Node], name: &str, src: &str) -> Option<SchemeCommand> {
    let mut lists = elements[1..].iter().filter(|n| n.kind() == "scheme_list");
    let arg_list = *lists.next()?;
    let predicate_list = *lists.next()?;

    let names: Vec<&str> = items_of_kind(arg_list, "scheme_symbol")
        .map(|n| text(n, src))
        .collect();
    let predicates: Vec<Predicate> = items(predicate_list)
        .into_iter()
        .filter_map(|n| predicate(n, src))
        .collect();

    // LilyPond up to 2.14 required every music function to declare `parser` and
    // `location` ahead of its real arguments, and plenty of files still in use
    // were written that way (and still work — LilyPond ignores the extras). The
    // predicate list has only ever described the *real* arguments, so the two
    // are aligned from the right: any surplus leading names are the legacy pair.
    let skip = names.len().saturating_sub(predicates.len());
    let params = predicates
        .iter()
        .enumerate()
        .map(|(i, predicate)| Param {
            name: names.get(skip + i).map_or_else(
                || Cow::Owned(format!("arg{}", i + 1)),
                |n| Cow::Owned((*n).to_string()),
            ),
            kind: arg_kind(predicate.name),
            optional: predicate.optional,
        })
        .collect();

    Some(SchemeCommand {
        name: name.to_string(),
        params,
        documentation: docstring(elements, predicate_list, src).map(|markdown| Documentation {
            markdown,
            source: DocSource::Workspace,
        }),
    })
}

/// One entry of a definition's predicate list.
struct Predicate<'a> {
    name: &'a str,
    /// LilyPond writes an optional argument as `(pred? default)`: a predicate
    /// paired with the value to use when the argument isn't there.
    optional: bool,
}

fn predicate<'a>(node: Node, src: &'a str) -> Option<Predicate<'a>> {
    match node.kind() {
        "scheme_symbol" => Some(Predicate {
            name: text(node, src),
            optional: false,
        }),
        "scheme_list" => items(node)
            .first()
            .filter(|n| n.kind() == "scheme_symbol")
            .map(|n| Predicate {
                name: text(*n, src),
                optional: true,
            }),
        _ => None,
    }
}

/// The source shape a predicate implies, for the handful whose shape we
/// actually know. Everything else consumes one node as an
/// [`ArgKind::Unknown`] named after the predicate, so signature help and hover
/// can still say what is expected even where the parser can only guess at its
/// extent.
fn arg_kind(predicate: &str) -> ArgKind {
    match predicate {
        "ly:music?" => ArgKind::Music,
        "ly:pitch?" => ArgKind::Pitch,
        "string?" => ArgKind::String,
        "integer?" | "index?" | "positive-integer?" | "non-negative-integer?" => ArgKind::Count,
        other => ArgKind::Unknown(Cow::Owned(other.to_string())),
    }
}

/// The docstring of a definition: the string immediately after its predicate
/// list, converted from Texinfo to Markdown once here rather than on every
/// hover. LilyPond's own definitions often wrap it in `(_i "…")` for gettext,
/// which is unwrapped to the same effect.
fn docstring(elements: &[Node], predicate_list: Node, src: &str) -> Option<String> {
    let after = elements
        .iter()
        .position(|n| n.id() == predicate_list.id())?
        .checked_add(1)?;
    let node = *elements.get(after)?;
    let string = match node.kind() {
        "scheme_string" => node,
        "scheme_list" => {
            let inner = items(node);
            let [head, string, ..] = inner.as_slice() else {
                return None;
            };
            if text(*head, src) != "_i" || string.kind() != "scheme_string" {
                return None;
            }
            *string
        }
        _ => return None,
    };
    let fragment = child_of_kind(string, "scheme_string_fragment")?;
    Some(texinfo_to_markdown(text(fragment, src)))
}

/// Converts a Texinfo docstring to Markdown: `@var{x}` and `@emph{x}` become
/// *x*, `@code{x}` and friends become `` `x` ``, `@bold{x}` becomes **x**, and
/// any other `@command{…}` keeps its contents unadorned. `@@`, `@{` and `@}`
/// are Texinfo's escapes for the three characters it reserves.
///
/// Deliberately shallow: this handles the inline markup that appears in
/// function docstrings, not Texinfo's block structure (`@example`, `@table`,
/// …), which docstrings don't use.
fn texinfo_to_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];

        let mut chars = rest.chars();
        match chars.next() {
            // The three escaped characters stand for themselves.
            Some(ch @ ('@' | '{' | '}')) => {
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
            Some(_) => {
                let word_end = rest
                    .find(|c: char| !c.is_ascii_alphanumeric())
                    .unwrap_or(rest.len());
                let (command, tail) = rest.split_at(word_end);
                let Some(body) = tail.strip_prefix('{').and_then(braced) else {
                    // A bare `@command` with no braces: nothing to wrap, so
                    // drop the `@` and carry on.
                    out.push_str(command);
                    rest = tail;
                    continue;
                };
                let inner = texinfo_to_markdown(body);
                match command {
                    "code" | "samp" | "file" | "kbd" | "key" => {
                        out.push('`');
                        out.push_str(&inner);
                        out.push('`');
                    }
                    "var" | "emph" | "i" => {
                        out.push('*');
                        out.push_str(&inner);
                        out.push('*');
                    }
                    "bold" | "b" | "strong" => {
                        out.push_str("**");
                        out.push_str(&inner);
                        out.push_str("**");
                    }
                    _ => out.push_str(&inner),
                }
                // Past the body and its closing brace.
                rest = &tail[body.len() + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// The contents of a brace group whose opening brace has already been
/// stripped, up to its matching close. `None` if the braces don't balance,
/// which leaves the text alone rather than swallowing the rest of the string.
fn braced(text: &str) -> Option<&str> {
    let mut depth = 1usize;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The meaningful elements of a Scheme list: its named children, minus
/// comments. The parentheses are anonymous nodes, so they fall away here.
fn items(list: Node) -> Vec<Node> {
    let mut cursor = list.walk();
    list.named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect()
}

fn items_of_kind<'a>(list: Node<'a>, kind: &'a str) -> impl Iterator<Item = Node<'a>> {
    items(list).into_iter().filter(move |n| n.kind() == kind)
}

fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|n| n.kind() == kind)
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.start_byte()..node.end_byte()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::definition;
    use crate::document;
    use crate::vocabulary::Layer;
    use proptest::prelude::*;

    /// Reads `src` as the server would.
    fn bindings(src: &str) -> Vec<Binding> {
        read(&document::parse(src, None), src)
    }

    /// The layer `src`'s Scheme alone makes — the document merges the
    /// assignment query's bindings in too, which these tests don't exercise.
    fn layer(src: &str) -> Layer {
        definition::layer(bindings(src))
    }

    /// The names `src` binds from inside Scheme, with the text each span
    /// covers, so a test can check *where* a definition was found and not only
    /// that it was.
    fn names(src: &str) -> Vec<(String, &str)> {
        bindings(src)
            .into_iter()
            .map(|binding| {
                let span = &src[binding.span.start..binding.span.end];
                (binding.name, span)
            })
            .collect()
    }

    /// The parameters of `\name` as `(name, kind, optional)` triples, for
    /// compact assertions.
    fn params(src: &str, name: &str) -> Vec<(String, ArgKind, bool)> {
        layer(src)
            .get(name)
            .unwrap_or_else(|| panic!("expected a definition of {name} in {src}"))
            .signature()
            .iter()
            .map(|p| (p.name.to_string(), p.kind.clone(), p.optional))
            .collect()
    }

    #[test]
    fn reads_an_assigned_music_function() {
        assert_eq!(
            params(
                "myFunc = #(define-music-function (music) (ly:music?) music)",
                "myFunc"
            ),
            vec![("music".to_string(), ArgKind::Music, false)]
        );
    }

    #[test]
    fn reads_the_legacy_parser_and_location_arguments_as_neither() {
        // Pre-2.15 definitions — of which there are plenty still in use —
        // declare `parser` and `location` before their real arguments, with
        // nothing for them in the predicate list. Aligning from the right
        // drops them.
        assert_eq!(
            params(
                "cnt = #(define-music-function (parser location note) (ly:music?) note)",
                "cnt"
            ),
            vec![("note".to_string(), ArgKind::Music, false)]
        );
    }

    #[test]
    fn reads_an_optional_argument_from_its_default() {
        assert_eq!(
            params(
                "myFunc = #(define-music-function (n music) ((integer? 3) ly:music?) music)",
                "myFunc"
            ),
            vec![
                ("n".to_string(), ArgKind::Count, true),
                ("music".to_string(), ArgKind::Music, false),
            ]
        );
    }

    #[test]
    fn an_unrecognised_predicate_keeps_its_name() {
        assert_eq!(
            params(
                "myFunc = #(define-music-function (x) (ly:duration?) x)",
                "myFunc"
            ),
            vec![(
                "x".to_string(),
                ArgKind::Unknown("ly:duration?".into()),
                false
            )]
        );
    }

    #[test]
    fn reads_a_function_bound_inside_scheme() {
        // No `assignment_lhs` anywhere: the name comes from the binding form.
        // This is the shape that used to produce a false-positive
        // undefined-reference diagnostic.
        let src = "#(define-public myFunc (define-music-function (m) (ly:music?) m))";
        assert_eq!(
            layer(src).get("myFunc").expect("myFunc").signature().len(),
            1
        );
        assert_eq!(names(src), vec![("myFunc".to_string(), "myFunc")]);
    }

    #[test]
    fn a_scheme_binding_defines_a_name_whatever_its_value() {
        // `\foo` resolves against the module the binding lands in, so the name
        // is defined even where nothing about its arguments is knowable. It
        // takes no arguments as far as we're concerned: a signature would be an
        // invention, and consuming nothing is what leaves whatever follows
        // `\foo` to be read as ordinary music.
        let src = "#(define-public answer 42)\n#(define myColour (x11-color 'red))\n";
        assert_eq!(
            names(src),
            vec![
                ("answer".to_string(), "answer"),
                ("myColour".to_string(), "myColour"),
            ]
        );
        let layer = layer(src);
        for name in ["answer", "myColour"] {
            assert!(layer.get(name).expect(name).signature().is_empty());
        }
    }

    #[test]
    fn an_assigned_definition_is_named_by_its_left_hand_side() {
        // `myFunc = #( … )` has an `assignment_lhs`, which the document's own
        // query captures too. Both name the same node, and it is that shared
        // span which lets the document keep one binding rather than two — and
        // so avoid doubling every go-to-definition result and rename edit.
        let src = "myFunc = #(define-music-function (m) (ly:music?) m)";
        assert_eq!(names(src), vec![("myFunc".to_string(), "myFunc")]);
        assert_eq!(bindings(src)[0].span, Span::new(0, 6));
    }

    #[test]
    fn event_scheme_and_void_functions_are_commands_too() {
        let src = "a = #(define-event-function (x) (number?) x)\n\
                   b = #(define-scheme-function (x) (string?) x)\n\
                   c = #(define-void-function () () (display 1))\n";
        let layer = layer(src);
        assert!(layer.get("a").is_some());
        assert!(layer.get("b").is_some());
        assert_eq!(layer.get("c").expect("c").signature().len(), 0);
    }

    #[test]
    fn ordinary_scheme_defines_no_music_function() {
        let src = "#(define (helper x) (* x 2))\n\
                   #(define-public answer 42)\n\
                   #(use-modules (lily accreg))\n\
                   plain = { c d e }\n";
        // Only the `answer` binding names anything `\foo` could reach: a
        // procedure's name is a list, and `use-modules` binds nothing. (`plain`
        // is an assignment, which the document's query reads, not this.)
        assert_eq!(names(src), vec![("answer".to_string(), "answer")]);
        assert!(
            layer(src)
                .get("answer")
                .expect("answer")
                .signature()
                .is_empty()
        );
    }

    #[test]
    fn a_half_typed_definition_is_no_definition() {
        // Mid-keystroke, before the predicate list exists. Reading must decline
        // rather than invent a signature that will change under the user.
        assert!(
            layer("myFunc = #(define-music-function (m)")
                .get("myFunc")
                .is_none()
        );
    }

    #[test]
    fn reads_a_docstring_as_markdown() {
        let src = "myFunc = #(define-music-function (m) (ly:music?)\n\
                   \x20 \"Repeat @var{m} twice, as @code{c4 c4}.\"\n\
                   \x20 m)";
        let layer = layer(src);
        let doc = layer
            .get("myFunc")
            .expect("myFunc")
            .documentation()
            .expect("a docstring");
        assert_eq!(doc.markdown, "Repeat *m* twice, as `c4 c4`.");
        assert!(matches!(doc.source, DocSource::Workspace));
    }

    #[test]
    fn unwraps_a_gettext_docstring() {
        let src = "myFunc = #(define-music-function (m) (ly:music?) (_i \"Do a thing.\") m)";
        let layer = layer(src);
        assert_eq!(
            layer
                .get("myFunc")
                .expect("myFunc")
                .documentation()
                .expect("a docstring")
                .markdown,
            "Do a thing."
        );
    }

    #[test]
    fn a_function_with_no_docstring_has_no_documentation() {
        let layer = layer("myFunc = #(define-music-function (m) (ly:music?) m)");
        assert!(
            layer
                .get("myFunc")
                .expect("myFunc")
                .documentation()
                .is_none()
        );
    }

    #[test]
    fn reads_a_real_library_of_accordion_functions() {
        // Lifted from a working library of the author's: four legacy-style
        // definitions whose bodies are `#{ … #}` LilyPond, one of them with a
        // nested `#( … )` inside that body, plus a `use-modules` call that
        // defines no command. The nested Scheme must not be mistaken for a
        // definition, and the `#{ … #}` must not be read at all.
        let src = "\
% indicate a counterbass note
cnt = #(define-music-function (parser location note) (ly:music?) #{ \\tweak NoteHead.color #darkgreen $note #})

min = #(define-music-function (parser location note) (ly:music?) #{ \\tweak NoteHead.color #(x11-color 'firebrick) $note #})

sev = #(define-music-function (parser location note) (ly:music?) #{ \\tweak NoteHead.color #blue $note #})
dimin = #(define-music-function (parser location note) (ly:music?) #{ \\tweak NoteHead.color #(x11-color 'BlueViolet) $note #})

#(use-modules (lily accreg))
";
        let layer = layer(src);
        assert_eq!(layer.len(), 4);
        for name in ["cnt", "min", "sev", "dimin"] {
            let command = layer.get(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(command.name(), name);
            assert_eq!(
                command
                    .signature()
                    .iter()
                    .map(|p| p.name.as_ref())
                    .collect::<Vec<_>>(),
                vec!["note"],
                "the legacy parser/location pair should not be parameters"
            );
        }
    }

    #[test]
    fn curried_and_plain_scheme_procedures_define_no_commands() {
        // Also from a real library: `#(define ((convolvelist rhythm) pitches) …)`
        // binds a procedure, not a music function — and its name is a list, not
        // a symbol, so there is nothing to call as `\foo` either way.
        let src = "\
#(define (convolve music)
  (let ((elems (ly:music-property music 'elements)))
    music))

#(define ((convolvelist rhythm) pitches)
  (map (lambda (p) p) pitches))
";
        assert_eq!(layer(src).len(), 0);
    }

    #[test]
    fn texinfo_markup_becomes_markdown() {
        // Kept as a literal pin of the specific conversions alongside the
        // property tests below, which cover the general shape rather than
        // these exact examples.
        assert_eq!(texinfo_to_markdown("plain text"), "plain text");
        assert_eq!(texinfo_to_markdown("@var{x} and @code{y}"), "*x* and `y`");
        assert_eq!(
            texinfo_to_markdown("@bold{very @var{bold}}"),
            "**very *bold***"
        );
        assert_eq!(texinfo_to_markdown("@unknown{kept}"), "kept");
        assert_eq!(texinfo_to_markdown("100@@ @{braced@}"), "100@ {braced}");
        // Unbalanced braces leave the text as it is rather than swallowing it.
        assert_eq!(texinfo_to_markdown("@code{oops"), "code{oops");
    }

    /// Text with no `@` and no braces at all, the alphabet `texinfo_to_markdown`
    /// has nothing to act on.
    fn plain_text_strategy() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9 ,.!?]{0,15}"
    }

    /// A mix of `@`, `{`, `}` and letters, dense enough to throw up real
    /// markup commands, escapes, and unbalanced or nested braces at random.
    fn markup_strategy() -> impl Strategy<Value = String> {
        "[@{}a-zA-Z]{0,30}"
    }

    proptest! {
        /// A docstring is workspace-authored text handed to us over LSP: no
        /// arrangement of `@`, `{` and `}` a user could type may crash the
        /// server, however malformed as Texinfo it is.
        #[test]
        fn texinfo_to_markdown_never_panics(text in markup_strategy()) {
            let _ = texinfo_to_markdown(&text);
        }

        /// With no `@` and no braces there is no markup to find, so the
        /// converter must be the identity — the base case the escape and
        /// command-handling branches all fall back to.
        #[test]
        fn texinfo_to_markdown_is_identity_without_markup(text in plain_text_strategy()) {
            prop_assert_eq!(texinfo_to_markdown(&text), text);
        }

        /// `@@`, `@{` and `@}` are Texinfo's escapes for the three characters
        /// it otherwise reserves; wrapping plain text around each escape
        /// must unescape to exactly that character and nothing else.
        #[test]
        fn escapes_decode_to_the_reserved_character(t in plain_text_strategy()) {
            prop_assert_eq!(texinfo_to_markdown(&format!("{t}@@{t}")), format!("{t}@{t}"));
            prop_assert_eq!(texinfo_to_markdown(&format!("{t}@{{{t}")), format!("{t}{{{t}"));
            prop_assert_eq!(texinfo_to_markdown(&format!("{t}@}}{t}")), format!("{t}}}{t}"));
        }

        /// A recognised `@command{...}` is always rewritten to its Markdown
        /// equivalent, so the literal Texinfo form must never survive into
        /// the output — *except* where the `@` itself was reintroduced by an
        /// `@@` escape rather than written as a command, which is excluded
        /// here since it is a different (and correctly handled) case.
        #[test]
        fn no_recognised_markup_command_survives(text in markup_strategy()) {
            prop_assume!(!text.contains("@@"));
            let markdown = texinfo_to_markdown(&text);
            let survived = ["@var{", "@code{", "@bold{"]
                .into_iter()
                .find(|marker| markdown.contains(marker));
            prop_assert_eq!(survived, None);
        }
    }
}
