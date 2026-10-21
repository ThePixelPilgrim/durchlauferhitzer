//! Lexer for the configuration language.

use std::iter::Peekable;
use std::str::Chars;

/// A 1-based position in the source text (column counts characters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Word(String),
    Colon,
    Semicolon,
    OpenBrace,
    CloseBrace,
}

/// Whether a token directly follows the previous one, or whitespace, a
/// comment or the start of input precedes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spacing {
    Joined,
    Separated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub spacing: Spacing,
    pub pos: Pos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LexErrorKind {
    UnexpectedChar(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LexError {
    pub kind: LexErrorKind,
    pub pos: Pos,
}

struct Cursor<'a> {
    chars: Peekable<Chars<'a>>,
    pos: Pos,
}

impl Cursor<'_> {
    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.pos.line = self.pos.line.saturating_add(1);
            self.pos.column = 1;
        } else {
            self.pos.column = self.pos.column.saturating_add(1);
        }
        Some(c)
    }
}

fn is_forbidden_control(c: char) -> bool {
    c.is_control() && !matches!(c, '\t' | '\n' | '\r')
}

fn is_delimiter(c: char) -> bool {
    matches!(c, ':' | ';' | '{' | '}' | '#')
}

fn error_at(c: char, pos: Pos) -> LexError {
    LexError {
        kind: LexErrorKind::UnexpectedChar(c),
        pos,
    }
}

/// Splits configuration text into tokens.
pub fn tokenize(text: &str) -> Result<Vec<Token>, LexError> {
    let mut cur = Cursor {
        chars: text.chars().peekable(),
        pos: Pos { line: 1, column: 1 },
    };
    let mut tokens = Vec::new();
    let mut spacing = Spacing::Separated;
    while let Some(c) = cur.peek() {
        let pos = cur.pos;
        if is_forbidden_control(c) {
            return Err(error_at(c, pos));
        }
        if c == '#' {
            while let Some(n) = cur.peek() {
                if n == '\n' {
                    break;
                }
                if is_forbidden_control(n) {
                    return Err(error_at(n, cur.pos));
                }
                cur.bump();
            }
            spacing = Spacing::Separated;
            continue;
        }
        if c.is_whitespace() {
            cur.bump();
            spacing = Spacing::Separated;
            continue;
        }
        let kind = match c {
            ':' => TokenKind::Colon,
            ';' => TokenKind::Semicolon,
            '{' => TokenKind::OpenBrace,
            '}' => TokenKind::CloseBrace,
            _ => {
                let mut word = String::new();
                while let Some(n) = cur.peek() {
                    if is_forbidden_control(n) {
                        return Err(error_at(n, cur.pos));
                    }
                    if n.is_whitespace() || is_delimiter(n) {
                        break;
                    }
                    word.push(n);
                    cur.bump();
                }
                tokens.push(Token {
                    kind: TokenKind::Word(word),
                    spacing,
                    pos,
                });
                spacing = Spacing::Joined;
                continue;
            }
        };
        cur.bump();
        tokens.push(Token { kind, spacing, pos });
        spacing = Spacing::Joined;
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::TokenKind::{CloseBrace, Colon, OpenBrace, Semicolon};
    use super::*;

    fn kinds(tokens: &[Token]) -> Vec<TokenKind> {
        tokens.iter().map(|t| t.kind.clone()).collect()
    }

    fn w(s: &str) -> TokenKind {
        TokenKind::Word(s.to_string())
    }

    #[test]
    fn tokenizes_setting_with_positions() {
        let t = tokenize("listen: 127.0.0.1:8080;").unwrap();
        // ":" inside a word is still a separator token
        assert_eq!(
            kinds(&t),
            vec![
                w("listen"),
                Colon,
                w("127.0.0.1"),
                Colon,
                w("8080"),
                Semicolon
            ]
        );
        assert_eq!(t[0].pos, Pos { line: 1, column: 1 });
        assert_eq!(t[2].pos, Pos { line: 1, column: 9 });
        assert_eq!(t[2].spacing, Spacing::Separated);
        assert_eq!(t[3].spacing, Spacing::Joined);
    }

    #[test]
    fn skips_comments_and_tracks_lines() {
        let t = tokenize("# c\nhost a.test {\n}\n").unwrap();
        assert_eq!(
            kinds(&t),
            vec![w("host"), w("a.test"), OpenBrace, CloseBrace]
        );
        assert_eq!(t[3].pos, Pos { line: 3, column: 1 });
    }

    #[test]
    fn rejects_control_characters() {
        let e = tokenize("a\u{0}b").unwrap_err();
        assert_eq!(
            e,
            LexError {
                kind: LexErrorKind::UnexpectedChar('\u{0}'),
                pos: Pos { line: 1, column: 2 }
            }
        );
    }

    #[test]
    fn hash_inside_word_starts_comment() {
        let t = tokenize("a#b").unwrap();
        assert_eq!(kinds(&t), vec![w("a")]);
    }

    #[test]
    fn hash_inside_word_comment_ends_at_newline() {
        let t = tokenize("a#b\nc").unwrap();
        assert_eq!(kinds(&t), vec![w("a"), w("c")]);
        assert_eq!(t[1].spacing, Spacing::Separated);
    }
}
