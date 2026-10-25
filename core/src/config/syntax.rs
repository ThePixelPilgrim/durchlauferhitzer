//! Syntax tree and recursive-descent parser for the configuration language.

use super::lexer::{LexErrorKind, Pos, Token, TokenKind, tokenize};

/// Maximum block nesting depth (a top-level block has depth 1).
const MAX_DEPTH: u32 = 8;

/// `key { value } ";"`. `values` may be empty (a flag); how many values a
/// key takes is checked by validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    pub key: String,
    pub values: Vec<String>,
    pub pos: Pos,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub name: String,
    pub args: Vec<String>,
    pub items: Vec<Item>,
    pub pos: Pos,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Setting(Setting),
    Block(Block),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyntaxErrorKind {
    Lex(LexErrorKind),
    UnexpectedToken,
    UnexpectedEnd,
    UnclosedBlock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntaxError {
    pub kind: SyntaxErrorKind,
    pub pos: Pos,
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.index).cloned();
        if token.is_some() {
            self.index += 1;
        }
        token
    }

    /// Position of the last token, used when input ends unexpectedly.
    fn last_pos(&self) -> Pos {
        self.tokens
            .last()
            .map_or(Pos { line: 1, column: 1 }, |t| t.pos)
    }

    /// Parses items until end of input or a `}`. The `}` is not consumed.
    /// `depth` is the number of enclosing blocks.
    fn parse_items(&mut self, depth: u32) -> Result<Vec<Item>, SyntaxError> {
        let mut items = Vec::new();
        loop {
            match self.peek().map(|t| &t.kind) {
                None | Some(TokenKind::CloseBrace) => return Ok(items),
                Some(TokenKind::Word(_)) => items.push(self.parse_item(depth)?),
                Some(TokenKind::Quoted(_) | TokenKind::Semicolon | TokenKind::OpenBrace) => {
                    return Err(self.unexpected_here());
                }
            }
        }
    }

    fn unexpected_here(&self) -> SyntaxError {
        SyntaxError {
            kind: SyntaxErrorKind::UnexpectedToken,
            pos: self.peek().map_or_else(|| self.last_pos(), |t| t.pos),
        }
    }

    fn parse_item(&mut self, depth: u32) -> Result<Item, SyntaxError> {
        let Some(Token {
            kind: TokenKind::Word(name),
            pos,
        }) = self.next()
        else {
            return Err(self.unexpected_here());
        };
        let mut values = Vec::new();
        loop {
            let Some(token) = self.next() else {
                return Err(SyntaxError {
                    kind: SyntaxErrorKind::UnexpectedEnd,
                    pos: self.last_pos(),
                });
            };
            match token.kind {
                TokenKind::Word(v) | TokenKind::Quoted(v) => values.push(v),
                TokenKind::Semicolon => {
                    // No values: a flag. Value counts are checked by
                    // validation, per key.
                    return Ok(Item::Setting(Setting {
                        key: name,
                        values,
                        pos,
                    }));
                }
                TokenKind::OpenBrace => {
                    if depth >= MAX_DEPTH {
                        return Err(SyntaxError {
                            kind: SyntaxErrorKind::UnexpectedToken,
                            pos,
                        });
                    }
                    let items = self.parse_items(depth + 1)?;
                    return match self.next() {
                        Some(Token {
                            kind: TokenKind::CloseBrace,
                            ..
                        }) => Ok(Item::Block(Block {
                            name,
                            args: values,
                            items,
                            pos,
                        })),
                        Some(_) | None => Err(SyntaxError {
                            kind: SyntaxErrorKind::UnclosedBlock,
                            pos,
                        }),
                    };
                }
                TokenKind::CloseBrace => {
                    return Err(SyntaxError {
                        kind: SyntaxErrorKind::UnexpectedToken,
                        pos: token.pos,
                    });
                }
            }
        }
    }
}

/// Parses configuration text into a list of top-level items.
pub fn parse(text: &str) -> Result<Vec<Item>, SyntaxError> {
    let tokens = tokenize(text).map_err(|e| SyntaxError {
        kind: SyntaxErrorKind::Lex(e.kind),
        pos: e.pos,
    })?;
    let mut parser = Parser { tokens, index: 0 };
    let items = parser.parse_items(0)?;
    if parser.peek().is_some() {
        // A `}` with no open block.
        return Err(parser.unexpected_here());
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = "\
listen 127.0.0.1:8080;
user www-data;            # drop privileges to this user after binding

limits {
  max_connections 1024;
  header_timeout 10s;
}

host example.com www.example.com {
  path / { static /srv/www; }
  path /api/ {
    proxy 127.0.0.1:3000;
    strip_prefix;
    max_request_body 10MiB;
    max_response_body unlimited;
  }
}
";

    fn err(text: &str) -> (SyntaxErrorKind, u32, u32) {
        match parse(text) {
            Err(e) => (e.kind, e.pos.line, e.pos.column),
            Ok(items) => panic!("expected error, got {items:?}"),
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn nested(n: usize) -> String {
        "a { ".repeat(n) + &"}".repeat(n)
    }

    #[test]
    fn parses_nested_blocks() {
        let items = parse(EXAMPLE).unwrap();
        assert_eq!(items.len(), 4);
        let Item::Block(host) = &items[3] else {
            panic!("host is not a block");
        };
        assert_eq!(host.name, "host");
        assert_eq!(host.args, strings(&["example.com", "www.example.com"]));
        assert_eq!(host.items.len(), 2);
        let Item::Block(api) = &host.items[1] else {
            panic!("api is not a block");
        };
        assert_eq!(api.name, "path");
        assert_eq!(api.args, strings(&["/api/"]));
        let Item::Setting(proxy) = &api.items[0] else {
            panic!("proxy is not a setting");
        };
        assert_eq!(proxy.key, "proxy");
        assert_eq!(proxy.values, strings(&["127.0.0.1:3000"]));
        let Item::Setting(strip) = &api.items[1] else {
            panic!("strip_prefix is not a setting");
        };
        assert_eq!(strip.key, "strip_prefix");
        assert_eq!(strip.values, Vec::<String>::new());
    }

    #[test]
    fn multiple_values() {
        let items = parse("listen [::1]:8080 127.0.0.1:80;").unwrap();
        let [Item::Setting(s)] = items.as_slice() else {
            panic!("expected one setting");
        };
        assert_eq!(s.values, strings(&["[::1]:8080", "127.0.0.1:80"]));
    }

    #[test]
    fn quoted_values() {
        let items = parse("a \"x y\" z;").unwrap();
        let [Item::Setting(s)] = items.as_slice() else {
            panic!("expected one setting");
        };
        assert_eq!(s.values, strings(&["x y", "z"]));
    }

    #[test]
    fn rejects_quoted_key() {
        assert_eq!(err("\"a\" b;"), (SyntaxErrorKind::UnexpectedToken, 1, 1));
    }

    #[test]
    fn reports_unclosed_block_at_its_start() {
        assert_eq!(err("a {\n b c;\n"), (SyntaxErrorKind::UnclosedBlock, 1, 1));
    }

    #[test]
    fn rejects_missing_semicolon() {
        assert_eq!(err("a { b c }"), (SyntaxErrorKind::UnexpectedToken, 1, 9));
    }

    #[test]
    fn flag_setting_has_no_values() {
        let items = parse("\na;").unwrap();
        assert_eq!(
            items,
            vec![Item::Setting(Setting {
                key: "a".to_string(),
                values: Vec::new(),
                pos: Pos { line: 2, column: 1 },
            })]
        );
    }

    #[test]
    fn rejects_stray_close_brace() {
        assert_eq!(err("a b; }"), (SyntaxErrorKind::UnexpectedToken, 1, 6));
    }

    #[test]
    fn rejects_input_ending_inside_setting() {
        assert_eq!(err("a b"), (SyntaxErrorKind::UnexpectedEnd, 1, 3));
    }

    #[test]
    fn maps_lex_errors() {
        assert_eq!(
            err("a \"b"),
            (SyntaxErrorKind::Lex(LexErrorKind::UnterminatedString), 1, 3)
        );
    }

    #[test]
    fn accepts_nesting_of_8() {
        assert!(parse(&nested(8)).is_ok());
    }

    #[test]
    fn rejects_nesting_deeper_than_8() {
        assert_eq!(err(&nested(9)), (SyntaxErrorKind::UnexpectedToken, 1, 33));
    }
}
