//! A minimal tokeniser for LilyPond source.
//!
//! This is deliberately shallow: it skips the things that would confuse a
//! naive scan (comments and string literals) and emits only the tokens the
//! analysis layer currently cares about. It carries enough structure that we
//! can grow it into a fuller parser later without rewriting the callers.

/// A half-open byte range `[start, end)` into the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn contains(&self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// A bare word, e.g. the `foo` in `foo = ...` (also matches note names).
    Identifier,
    /// A backslash command, e.g. `\foo`. The span covers the whole `\foo`.
    Command,
    /// A single `=`.
    Equals,
    OpenBrace,
    CloseBrace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// Returns whether `c` may appear in an identifier or command name.
///
/// LilyPond's lexer treats variable and command names as runs of alphabetic
/// characters. Kept as a single predicate so the rule is easy to widen later
/// (e.g. to admit digits or hyphens) without hunting through the scanner.
fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphabetic()
}

/// Tokenises `src`, skipping whitespace, comments and string literals.
pub fn tokenise(src: &str) -> Vec<Token> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            // Block comment `%{ ... %}` or line comment `% ...`.
            '%' => {
                if bytes.get(i + 1) == Some(&b'{') {
                    i = skip_block_comment(bytes, i + 2);
                } else {
                    i = skip_line_comment(bytes, i + 1);
                }
            }
            // String literal; contents are opaque to us.
            '"' => i = skip_string(bytes, i + 1),
            // A backslash command, unless the backslash is something else
            // (e.g. `\\`, `\!`, `\>`), in which case we just step over it.
            '\\' => {
                let name_start = i + 1;
                if bytes
                    .get(name_start)
                    .is_some_and(|&b| is_identifier_char(b as char))
                {
                    let end = scan_identifier(bytes, name_start);
                    tokens.push(Token {
                        kind: TokenKind::Command,
                        span: Span::new(i, end),
                    });
                    i = end;
                } else {
                    i += 1;
                }
            }
            '=' => {
                tokens.push(Token {
                    kind: TokenKind::Equals,
                    span: Span::new(i, i + 1),
                });
                i += 1;
            }
            '{' => {
                tokens.push(Token {
                    kind: TokenKind::OpenBrace,
                    span: Span::new(i, i + 1),
                });
                i += 1;
            }
            '}' => {
                tokens.push(Token {
                    kind: TokenKind::CloseBrace,
                    span: Span::new(i, i + 1),
                });
                i += 1;
            }
            _ if is_identifier_char(c) => {
                let end = scan_identifier(bytes, i);
                tokens.push(Token {
                    kind: TokenKind::Identifier,
                    span: Span::new(i, end),
                });
                i = end;
            }
            // Whitespace and anything else we don't model yet.
            _ => i += 1,
        }
    }

    tokens
}

/// Returns the byte offset just past the run of identifier characters that
/// starts at `start`.
fn scan_identifier(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && is_identifier_char(bytes[i] as char) {
        i += 1;
    }
    i
}

/// Skips to just past the next newline (or end of input).
fn skip_line_comment(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

/// Skips to just past the closing `%}` (or end of input). `from` is the offset
/// after the opening `%{`.
fn skip_block_comment(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'%' && bytes.get(i + 1) == Some(&b'}') {
            return i + 2;
        }
        i += 1;
    }
    i
}

/// Skips to just past the closing `"` (or end of input). Honours `\"` escapes.
/// `from` is the offset after the opening quote.
fn skip_string(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2, // skip the escaped character
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: collect (kind, matched text) pairs for assertions.
    fn lex(src: &str) -> Vec<(TokenKind, &str)> {
        tokenise(src)
            .into_iter()
            .map(|t| (t.kind, &src[t.span.start..t.span.end]))
            .collect()
    }

    #[test]
    fn definition_and_reference() {
        use TokenKind::*;
        assert_eq!(
            lex("foo = { c d e }\n\\foo"),
            vec![
                (Identifier, "foo"),
                (Equals, "="),
                (OpenBrace, "{"),
                (Identifier, "c"),
                (Identifier, "d"),
                (Identifier, "e"),
                (CloseBrace, "}"),
                (Command, "\\foo"),
            ]
        );
    }

    #[test]
    fn line_comment_is_skipped() {
        use TokenKind::*;
        assert_eq!(lex("a % foo = bar \\baz\nb"), vec![(Identifier, "a"), (Identifier, "b")]);
    }

    #[test]
    fn block_comment_is_skipped() {
        use TokenKind::*;
        assert_eq!(lex("a %{ foo = \\bar\n still in %} b"), vec![(Identifier, "a"), (Identifier, "b")]);
    }

    #[test]
    fn string_contents_are_opaque() {
        use TokenKind::*;
        // The `=`, `\foo` and braces inside the string must not be tokenised.
        assert_eq!(
            lex(r#"title = "a = \foo { }""#),
            vec![(Identifier, "title"), (Equals, "=")]
        );
    }

    #[test]
    fn escaped_quote_in_string() {
        use TokenKind::*;
        assert_eq!(lex(r#""he said \"hi\"" x"#), vec![(Identifier, "x")]);
    }

    #[test]
    fn non_command_backslashes() {
        // `\\`, `\!` and `\>` are not commands and produce no tokens.
        assert!(lex(r"\\ \! \>").is_empty());
    }

    #[test]
    fn command_name_stops_at_non_alpha() {
        use TokenKind::*;
        // Digits aren't identifier characters, so they're dropped here.
        assert_eq!(lex(r"\foo123"), vec![(Command, "\\foo")]);
    }
}
