//! Tokenizer for Verilog source text.
//!
//! Compiler directives (`` `name ``) and line ends are kept as tokens so that
//! the preprocessor can handle them; the preprocessor removes them before
//! parsing.

use synth_core::{Diagnostic, FileId, Span};

/// A token kind. Identifiers and keywords share [`Tok::Ident`]; the parser
/// distinguishes keywords.
#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    /// `$name` (system task or function).
    SysIdent(String),
    /// `` `name `` (compiler directive or macro use).
    Directive(String),
    /// An unsized decimal number (also the size part of a sized literal).
    Decimal(String),
    /// A based literal, possibly sized: `8'hff`, `'sb101`.
    Based {
        size: Option<u32>,
        signed: bool,
        base: char,
        digits: String,
    },
    Real(String),
    Str(String),
    /// Punctuation and operators, e.g. `(`, `<=`, `===`, `+:`.
    Op(&'static str),
    /// `(*` attribute start; `*)` attribute end.
    AttrOpen,
    AttrClose,
    Newline,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

const OPS: &[&str] = &[
    // Longest first.
    "<<<=", ">>>=", "===", "!==", "<<<", ">>>", "<<=", ">>=", "+:", "-:", "**", "==", "!=", "<=",
    ">=", "&&", "||", "<<", ">>", "~&", "~|", "~^", "^~", "->", "+=", "-=", "++", "--", "::", "(",
    ")", "[", "]", "{", "}", ";", ",", ".", ":", "?", "=", "+", "-", "*", "/", "%", "<", ">", "!",
    "~", "&", "|", "^", "@", "#",
];

/// Splits `text` into tokens. Errors are reported and lexing continues.
pub fn lex(file: FileId, text: &str, diags: &mut Vec<Diagnostic>) -> Vec<Token> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let sp = |s: usize, e: usize| Span::new(file, s, e);
    while i < b.len() {
        let c = b[i];
        let start = i;
        match c {
            b'\n' => {
                out.push(Token { tok: Tok::Newline, span: sp(i, i + 1) });
                i += 1;
            }
            b' ' | b'\t' | b'\r' | 0x0c => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    // Keep line ends inside comments visible to the preprocessor.
                    if b[i] == b'\n' {
                        out.push(Token { tok: Tok::Newline, span: sp(i, i + 1) });
                    }
                    i += 1;
                }
                if i >= b.len() {
                    diags.push(Diagnostic::error(
                        sp(start, start + 2),
                        "unterminated block comment",
                    ));
                }
                i = (i + 2).min(b.len());
            }
            b'(' if b.get(i + 1) == Some(&b'*') && b.get(i + 2) != Some(&b')') => {
                out.push(Token { tok: Tok::AttrOpen, span: sp(i, i + 2) });
                i += 2;
            }
            b'*' if b.get(i + 1) == Some(&b')')
                && out.last().is_none_or(|t| t.tok != Tok::Op("(")) =>
            {
                out.push(Token { tok: Tok::AttrClose, span: sp(i, i + 2) });
                i += 2;
            }
            b'"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match b.get(i) {
                        None | Some(b'\n') => {
                            diags.push(Diagnostic::error(sp(start, i), "unterminated string"));
                            break;
                        }
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(b'\\') => {
                            let esc = b.get(i + 1).copied().unwrap_or(b'\\');
                            match esc {
                                b'n' => s.push('\n'),
                                b't' => s.push('\t'),
                                b'\\' => s.push('\\'),
                                b'"' => s.push('"'),
                                b'\n' => {}
                                b'0'..=b'7' => {
                                    let mut v = 0u32;
                                    let mut k = 0;
                                    while k < 3 && matches!(b.get(i + 1 + k), Some(b'0'..=b'7')) {
                                        v = v * 8 + (b[i + 1 + k] - b'0') as u32;
                                        k += 1;
                                    }
                                    s.push(char::from_u32(v).unwrap_or('?'));
                                    i += k - 1;
                                }
                                other => s.push(other as char),
                            }
                            i += 2;
                        }
                        Some(_) => {
                            let ch = text[i..].chars().next().unwrap();
                            s.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                out.push(Token { tok: Tok::Str(s), span: sp(start, i) });
            }
            b'`' => {
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$')
                {
                    i += 1;
                }
                let name = text[start + 1..i].to_string();
                if name.is_empty() {
                    // `` `" `` and `` `` `` (stringification, pasting) are unsupported.
                    diags.push(Diagnostic::error(sp(start, i + 1), "unsupported macro operator"));
                    i += 1;
                } else {
                    out.push(Token { tok: Tok::Directive(name), span: sp(start, i) });
                }
            }
            b'\\' if matches!(b.get(i + 1), Some(b'\n' | b'\r')) => {
                // Line continuation (in macro definitions).
                i += 1;
                if b.get(i) == Some(&b'\r') {
                    i += 1;
                }
                if b.get(i) == Some(&b'\n') {
                    i += 1;
                }
            }
            b'\\' => {
                // Escaped identifier: up to white space.
                i += 1;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                out.push(Token {
                    tok: Tok::Ident(text[start + 1..i].to_string()),
                    span: sp(start, i),
                });
            }
            b'$' if b.get(i + 1).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') => {
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$')
                {
                    i += 1;
                }
                out.push(Token {
                    tok: Tok::SysIdent(text[start + 1..i].to_string()),
                    span: sp(start, i),
                });
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$')
                {
                    i += 1;
                }
                out.push(Token { tok: Tok::Ident(text[start..i].to_string()), span: sp(start, i) });
            }
            _ if c.is_ascii_digit() => {
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                    i += 1;
                }
                // Real number?
                let is_real_frac =
                    b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit);
                let is_exp = matches!(b.get(i), Some(b'e' | b'E'))
                    && (b.get(i + 1).is_some_and(u8::is_ascii_digit)
                        || (matches!(b.get(i + 1), Some(b'+' | b'-'))
                            && b.get(i + 2).is_some_and(u8::is_ascii_digit)));
                if is_real_frac || is_exp {
                    if is_real_frac {
                        i += 1;
                        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                            i += 1;
                        }
                    }
                    if matches!(b.get(i), Some(b'e' | b'E')) {
                        i += 1;
                        if matches!(b.get(i), Some(b'+' | b'-')) {
                            i += 1;
                        }
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                    out.push(Token {
                        tok: Tok::Real(text[start..i].replace('_', "")),
                        span: sp(start, i),
                    });
                    continue;
                }
                let digits = text[start..i].replace('_', "");
                // Sized based literal: size, optional white space, then `'base`.
                let mut j = i;
                while j < b.len() && matches!(b[j], b' ' | b'\t') {
                    j += 1;
                }
                if b.get(j) == Some(&b'\'') && based_follows(b, j + 1) {
                    let size: Option<u32> = digits.parse().ok();
                    if size.is_none() || size == Some(0) {
                        diags.push(Diagnostic::error(sp(start, i), "invalid literal size"));
                    }
                    i = j;
                    let (tok, end) = lex_based(text, j, size.filter(|&s| s > 0), file, diags);
                    out.push(Token { tok, span: sp(start, end) });
                    i = i.max(end);
                } else {
                    out.push(Token { tok: Tok::Decimal(digits), span: sp(start, i) });
                }
            }
            b'\'' if based_follows(b, i + 1) => {
                let (tok, end) = lex_based(text, i, None, file, diags);
                out.push(Token { tok, span: sp(start, end) });
                i = end;
            }
            b'\'' if matches!(b.get(i + 1), Some(b'0' | b'1' | b'x' | b'X' | b'z' | b'Z')) => {
                // SystemVerilog unbased unsized literal ('0, '1, 'x, 'z).
                let d = (b[i + 1] as char).to_ascii_lowercase();
                out.push(Token {
                    tok: Tok::Based { size: None, signed: false, base: 'u', digits: d.to_string() },
                    span: sp(i, i + 2),
                });
                i += 2;
            }
            _ => {
                let rest = &text[i..];
                if let Some(op) = OPS.iter().find(|op| rest.starts_with(**op)) {
                    out.push(Token { tok: Tok::Op(op), span: sp(i, i + op.len()) });
                    i += op.len();
                } else {
                    let ch = rest.chars().next().unwrap();
                    diags.push(Diagnostic::error(
                        sp(i, i + ch.len_utf8()),
                        format!("unexpected character `{ch}`"),
                    ));
                    i += ch.len_utf8();
                }
            }
        }
    }
    out.push(Token { tok: Tok::Eof, span: sp(b.len(), b.len()) });
    out
}

fn based_follows(b: &[u8], i: usize) -> bool {
    let mut k = i;
    if matches!(b.get(k), Some(b's' | b'S')) {
        k += 1;
    }
    matches!(b.get(k), Some(b'b' | b'B' | b'o' | b'O' | b'd' | b'D' | b'h' | b'H'))
}

/// Lexes `'[s]<base><digits>` starting at the apostrophe at `i`.
fn lex_based(
    text: &str,
    i: usize,
    size: Option<u32>,
    file: FileId,
    diags: &mut Vec<Diagnostic>,
) -> (Tok, usize) {
    let b = text.as_bytes();
    let mut k = i + 1;
    let signed = matches!(b.get(k), Some(b's' | b'S'));
    if signed {
        k += 1;
    }
    let base = (b[k] as char).to_ascii_lowercase();
    k += 1;
    while k < b.len() && matches!(b[k], b' ' | b'\t') {
        k += 1;
    }
    let dstart = k;
    while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'_' || b[k] == b'?') {
        k += 1;
    }
    let digits: String = text[dstart..k].chars().filter(|&c| c != '_').collect();
    if digits.is_empty() {
        diags.push(Diagnostic::error(Span::new(file, i, k), "missing digits in based literal"));
    }
    (Tok::Based { size, signed, base, digits }, k)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        let mut d = Vec::new();
        let t = lex(FileId(0), s, &mut d);
        assert!(d.is_empty(), "{d:?}");
        t.into_iter().map(|t| t.tok).filter(|t| *t != Tok::Newline && *t != Tok::Eof).collect()
    }

    #[test]
    fn numbers() {
        assert_eq!(
            toks("8'hFF 4 'b10_10 'sd5 12 1.5 3e2"),
            vec![
                Tok::Based { size: Some(8), signed: false, base: 'h', digits: "FF".into() },
                Tok::Based { size: Some(4), signed: false, base: 'b', digits: "1010".into() },
                Tok::Based { size: None, signed: true, base: 'd', digits: "5".into() },
                Tok::Decimal("12".into()),
                Tok::Real("1.5".into()),
                Tok::Real("3e2".into()),
            ]
        );
    }

    #[test]
    fn operators_and_misc() {
        assert_eq!(
            toks("a <= b >>> 2; x[3 +: 4] (* keep *) \\esc$id  $display `FOO // c\n/* x */"),
            vec![
                Tok::Ident("a".into()),
                Tok::Op("<="),
                Tok::Ident("b".into()),
                Tok::Op(">>>"),
                Tok::Decimal("2".into()),
                Tok::Op(";"),
                Tok::Ident("x".into()),
                Tok::Op("["),
                Tok::Decimal("3".into()),
                Tok::Op("+:"),
                Tok::Decimal("4".into()),
                Tok::Op("]"),
                Tok::AttrOpen,
                Tok::Ident("keep".into()),
                Tok::AttrClose,
                Tok::Ident("esc$id".into()),
                Tok::SysIdent("display".into()),
                Tok::Directive("FOO".into()),
            ]
        );
        // `@(*)` is an event control, not an attribute.
        assert_eq!(toks("@(*)"), vec![Tok::Op("@"), Tok::Op("("), Tok::Op("*"), Tok::Op(")")]);
    }
}
