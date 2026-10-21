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
    /// A double-quoted string, holding its unescaped contents.
    Quoted(String),
    Semicolon,
    OpenBrace,
    CloseBrace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub pos: Pos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LexErrorKind {
    UnexpectedChar(char),
    UnterminatedString,
    BadEscape(char),
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
    matches!(c, ';' | '{' | '}' | '#' | '"')
}

/// Reads a quoted string; the cursor is on the opening quote.
fn read_quoted(cur: &mut Cursor<'_>, open: Pos) -> Result<String, LexError> {
    let unterminated = LexError {
        kind: LexErrorKind::UnterminatedString,
        pos: open,
    };
    cur.bump();
    let mut out = String::new();
    loop {
        let at = cur.pos;
        let Some(n) = cur.peek() else {
            return Err(unterminated);
        };
        if n == '\n' {
            return Err(unterminated);
        }
        if is_forbidden_control(n) {
            return Err(error_at(n, at));
        }
        cur.bump();
        match n {
            '"' => return Ok(out),
            '\\' => {
                let Some(e) = cur.peek() else {
                    return Err(unterminated);
                };
                if e == '\n' {
                    return Err(unterminated);
                }
                if is_forbidden_control(e) {
                    return Err(error_at(e, cur.pos));
                }
                cur.bump();
                match e {
                    '"' | '\\' => out.push(e),
                    other => {
                        return Err(LexError {
                            kind: LexErrorKind::BadEscape(other),
                            pos: at,
                        });
                    }
                }
            }
            other => out.push(other),
        }
    }
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
            continue;
        }
        if c.is_whitespace() {
            cur.bump();
            continue;
        }
        let kind = match c {
            ';' => TokenKind::Semicolon,
            '{' => TokenKind::OpenBrace,
            '}' => TokenKind::CloseBrace,
            '"' => {
                let text = read_quoted(&mut cur, pos)?;
                tokens.push(Token {
                    kind: TokenKind::Quoted(text),
                    pos,
                });
                continue;
            }
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
                    pos,
                });
                continue;
            }
        };
        cur.bump();
        tokens.push(Token { kind, pos });
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::TokenKind::{CloseBrace, OpenBrace, Semicolon};
    use super::*;

    fn kinds(tokens: &[Token]) -> Vec<TokenKind> {
        tokens.iter().map(|t| t.kind.clone()).collect()
    }

    fn w(s: &str) -> TokenKind {
        TokenKind::Word(s.to_string())
    }

    #[test]
    fn tokenizes_setting_with_positions() {
        let t = tokenize("listen 127.0.0.1:8080 [::1]:80;").unwrap();
        assert_eq!(
            kinds(&t),
            vec![w("listen"), w("127.0.0.1:8080"), w("[::1]:80"), Semicolon]
        );
        assert_eq!(t[0].pos, Pos { line: 1, column: 1 });
        assert_eq!(t[1].pos, Pos { line: 1, column: 8 });
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
    }

    fn q(s: &str) -> TokenKind {
        TokenKind::Quoted(s.to_string())
    }

    #[test]
    fn quoted_string_keeps_special_characters() {
        let t = tokenize(r#"a "x#y; {z}";"#).unwrap();
        assert_eq!(kinds(&t), vec![w("a"), q("x#y; {z}"), Semicolon]);
    }

    #[test]
    fn quoted_string_escapes() {
        let t = tokenize(r#""a\"b\\c""#).unwrap();
        assert_eq!(kinds(&t), vec![q("a\"b\\c")]);
    }

    #[test]
    fn unterminated_string_reports_opening_quote() {
        let e = tokenize("a \"bc\nd\"").unwrap_err();
        assert_eq!(
            e,
            LexError {
                kind: LexErrorKind::UnterminatedString,
                pos: Pos { line: 1, column: 3 }
            }
        );
        let e = tokenize("a \"bc").unwrap_err();
        assert_eq!(e.kind, LexErrorKind::UnterminatedString);
        assert_eq!(e.pos, Pos { line: 1, column: 3 });
    }

    #[test]
    fn bad_escape_reports_backslash() {
        let e = tokenize(r#""a\nb""#).unwrap_err();
        assert_eq!(
            e,
            LexError {
                kind: LexErrorKind::BadEscape('n'),
                pos: Pos { line: 1, column: 3 }
            }
        );
    }

    #[test]
    fn quote_ends_word() {
        let t = tokenize(r#"ab"c""#).unwrap();
        assert_eq!(kinds(&t), vec![w("ab"), q("c")]);
    }

    #[test]
    fn control_character_in_string_is_rejected() {
        let e = tokenize("\"a\u{0}b\"").unwrap_err();
        assert_eq!(e.kind, LexErrorKind::UnexpectedChar('\u{0}'));
        assert_eq!(e.pos, Pos { line: 1, column: 3 });
    }
}
