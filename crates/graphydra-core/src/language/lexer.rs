//! Tokenizer for GHQL.

use crate::error::{GraphError, SyntaxError};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// Bare word; keywords are recognised by the parser case-insensitively.
    Word(String),
    /// `backtick-quoted` identifier (never a keyword).
    Quoted(String),
    Str(String),
    Int(i64),
    Float(f64),
    Param(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Dot,
    DotDot,
    Colon,
    Semi,
    Pipe,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Caret,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    /// `->`
    Arrow,
    /// `<-`
    LArrow,
    PlusEq,
    Eof,
}

impl Tok {
    pub(crate) fn describe(&self) -> String {
        match self {
            Tok::Word(w) => format!("'{w}'"),
            Tok::Quoted(w) => format!("`{w}`"),
            Tok::Str(_) => "string literal".into(),
            Tok::Int(i) => format!("'{i}'"),
            Tok::Float(f) => format!("'{f}'"),
            Tok::Param(p) => format!("'${p}'"),
            Tok::Eof => "end of input".into(),
            other => format!("'{}'", punct(other)),
        }
    }
}

fn punct(t: &Tok) -> &'static str {
    match t {
        Tok::LParen => "(",
        Tok::RParen => ")",
        Tok::LBracket => "[",
        Tok::RBracket => "]",
        Tok::LBrace => "{",
        Tok::RBrace => "}",
        Tok::Comma => ",",
        Tok::Dot => ".",
        Tok::DotDot => "..",
        Tok::Colon => ":",
        Tok::Semi => ";",
        Tok::Pipe => "|",
        Tok::Star => "*",
        Tok::Plus => "+",
        Tok::Minus => "-",
        Tok::Slash => "/",
        Tok::Percent => "%",
        Tok::Caret => "^",
        Tok::Eq => "=",
        Tok::Neq => "<>",
        Tok::Lt => "<",
        Tok::Le => "<=",
        Tok::Gt => ">",
        Tok::Ge => ">=",
        Tok::Arrow => "->",
        Tok::LArrow => "<-",
        Tok::PlusEq => "+=",
        _ => "?",
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub(crate) tok: Tok,
    /// Byte offsets into the source.
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Build a positioned syntax error for byte offset `at`.
pub(crate) fn syntax_error(src: &str, at: usize, message: impl Into<String>) -> GraphError {
    let at = at.min(src.len());
    let before = &src[..at];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = src[line_start..at].chars().count() + 1;
    let line_end = src[line_start..].find('\n').map_or(src.len(), |i| line_start + i);
    let snippet = src[line_start..line_end].trim_end_matches('\r').to_owned();
    GraphError::Syntax(SyntaxError { message: message.into(), line, column, snippet })
}

pub(crate) fn tokenize(src: &str) -> Result<Vec<Token>, GraphError> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        // Whitespace
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Comments
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            loop {
                if i + 1 >= bytes.len() {
                    return Err(syntax_error(src, start, "unterminated block comment"));
                }
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        let start = i;
        let push = |out: &mut Vec<Token>, tok: Tok, end: usize| out.push(Token { tok, start, end });

        // Identifiers / keywords (Unicode letters allowed).
        let ch = src[i..].chars().next().expect("in bounds");
        if ch.is_alphabetic() || ch == '_' {
            let mut j = i;
            for (off, ch) in src[i..].char_indices() {
                if ch.is_alphanumeric() || ch == '_' {
                    j = i + off + ch.len_utf8();
                } else {
                    break;
                }
            }
            push(&mut out, Tok::Word(src[i..j].to_owned()), j);
            i = j;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let (tok, j) = lex_number(src, i)?;
            push(&mut out, tok, j);
            i = j;
            continue;
        }
        match c {
            b'\'' | b'"' => {
                let (s, j) = lex_string(src, i)?;
                push(&mut out, Tok::Str(s), j);
                i = j;
            }
            b'`' => {
                let Some(rel) = src[i + 1..].find('`') else {
                    return Err(syntax_error(src, i, "unterminated quoted identifier"));
                };
                let name = &src[i + 1..i + 1 + rel];
                if name.is_empty() {
                    return Err(syntax_error(src, i, "empty quoted identifier"));
                }
                push(&mut out, Tok::Quoted(name.to_owned()), i + rel + 2);
                i += rel + 2;
            }
            b'$' => {
                let mut j = i + 1;
                for (off, ch) in src[i + 1..].char_indices() {
                    if ch.is_alphanumeric() || ch == '_' {
                        j = i + 1 + off + ch.len_utf8();
                    } else {
                        break;
                    }
                }
                if j == i + 1 {
                    return Err(syntax_error(src, i, "expected parameter name after '$'"));
                }
                push(&mut out, Tok::Param(src[i + 1..j].to_owned()), j);
                i = j;
            }
            _ => {
                let two = bytes.get(i..i + 2);
                let (tok, len) = match two {
                    Some(b"->") => (Tok::Arrow, 2),
                    Some(b"<-") => (Tok::LArrow, 2),
                    Some(b"<=") => (Tok::Le, 2),
                    Some(b">=") => (Tok::Ge, 2),
                    Some(b"<>") | Some(b"!=") => (Tok::Neq, 2),
                    Some(b"..") => (Tok::DotDot, 2),
                    Some(b"+=") => (Tok::PlusEq, 2),
                    _ => (
                        match c {
                            b'(' => Tok::LParen,
                            b')' => Tok::RParen,
                            b'[' => Tok::LBracket,
                            b']' => Tok::RBracket,
                            b'{' => Tok::LBrace,
                            b'}' => Tok::RBrace,
                            b',' => Tok::Comma,
                            b'.' => Tok::Dot,
                            b':' => Tok::Colon,
                            b';' => Tok::Semi,
                            b'|' => Tok::Pipe,
                            b'*' => Tok::Star,
                            b'+' => Tok::Plus,
                            b'-' => Tok::Minus,
                            b'/' => Tok::Slash,
                            b'%' => Tok::Percent,
                            b'^' => Tok::Caret,
                            b'=' => Tok::Eq,
                            b'<' => Tok::Lt,
                            b'>' => Tok::Gt,
                            _ => {
                                return Err(syntax_error(src, i, format!("unexpected character {ch:?}")));
                            }
                        },
                        1,
                    ),
                };
                push(&mut out, tok, i + len);
                i += len;
            }
        }
    }
    out.push(Token { tok: Tok::Eof, start: src.len(), end: src.len() });
    Ok(out)
}

fn lex_number(src: &str, start: usize) -> Result<(Tok, usize), GraphError> {
    let bytes = src.as_bytes();
    let mut i = start;
    let mut is_float = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // A '.' is part of the number only when followed by a digit, so that
    // `1..3` lexes as Int DotDot Int.
    if i < bytes.len() && bytes[i] == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
        is_float = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        if j < bytes.len() && bytes[j].is_ascii_digit() {
            is_float = true;
            i = j;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    if i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        return Err(syntax_error(src, i, "invalid character in number"));
    }
    let text = &src[start..i];
    if is_float {
        let f: f64 = text.parse().map_err(|_| syntax_error(src, start, "invalid number"))?;
        if !f.is_finite() {
            return Err(syntax_error(src, start, "number is out of range"));
        }
        Ok((Tok::Float(f), i))
    } else {
        let v: i64 = text.parse().map_err(|_| syntax_error(src, start, "integer is out of the 64-bit range"))?;
        Ok((Tok::Int(v), i))
    }
}

fn lex_string(src: &str, start: usize) -> Result<(String, usize), GraphError> {
    let quote = src.as_bytes()[start] as char;
    let mut out = String::new();
    let mut chars = src[start + 1..].char_indices();
    while let Some((off, ch)) = chars.next() {
        let pos = start + 1 + off;
        if ch == quote {
            return Ok((out, pos + 1));
        }
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some((_, esc)) = chars.next() else { break };
        match esc {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            '0' => out.push('\0'),
            '\\' => out.push('\\'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            'u' => {
                let hex: String = (0..4).filter_map(|_| chars.next().map(|(_, c)| c)).collect();
                let cp = u32::from_str_radix(&hex, 16)
                    .ok()
                    .filter(|_| hex.len() == 4)
                    .and_then(char::from_u32)
                    .ok_or_else(|| syntax_error(src, pos, "invalid \\u escape (expected 4 hex digits)"))?;
                out.push(cp);
            }
            other => return Err(syntax_error(src, pos, format!("unknown escape sequence '\\{other}'"))),
        }
    }
    Err(syntax_error(src, start, "unterminated string literal"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        tokenize(s).expect("lex").into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn lexes_patterns_and_ranges() {
        assert_eq!(
            toks("(a)-[:T*1..3]->(b)"),
            vec![
                Tok::LParen,
                Tok::Word("a".into()),
                Tok::RParen,
                Tok::Minus,
                Tok::LBracket,
                Tok::Colon,
                Tok::Word("T".into()),
                Tok::Star,
                Tok::Int(1),
                Tok::DotDot,
                Tok::Int(3),
                Tok::RBracket,
                Tok::Arrow,
                Tok::LParen,
                Tok::Word("b".into()),
                Tok::RParen,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn lexes_literals() {
        assert_eq!(
            toks(r#"'it\'s' "aA" 1.5e3 .5 42 $p `we ird`"#),
            vec![
                Tok::Str("it's".into()),
                Tok::Str("aA".into()),
                Tok::Float(1500.0),
                Tok::Float(0.5),
                Tok::Int(42),
                Tok::Param("p".into()),
                Tok::Quoted("we ird".into()),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn reports_positions() {
        let err = tokenize("MATCH (n)\nRETURN 'abc").expect_err("unterminated");
        match err {
            GraphError::Syntax(e) => {
                assert_eq!((e.line, e.column), (2, 8));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(tokenize("RETURN 99999999999999999999").is_err());
        assert!(tokenize("RETURN 1 # 2").is_err());
    }
}
