//! Tokens of Weft source text.

use synth_core::{BigInt, Diagnostic, Diagnostics, FileId, Span};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    /// An integer literal. Binary and hexadecimal literals carry their
    /// natural width (digits × 1 or 4).
    Int(BigInt, Option<u32>),
    /// A binary literal with `?` digits: (value, care mask, width), most
    /// significant digit first in the source.
    Bits(u128, u128, u32),
    Str(String),
    Punct(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Int(v, _) => format!("number `{v}`"),
            Tok::Bits(..) => "bit pattern".into(),
            Tok::Str(_) => "string".into(),
            Tok::Punct(p) => format!("`{p}`"),
            Tok::Eof => "end of file".into(),
        }
    }
}

// Longest first.
const PUNCTS: &[&str] = &[
    "..=", "+%", "-%", "*%", "**", "&&", "||", "<<", ">>", "==", "!=", "<=", ">=", ":=", "=>",
    "->", "..", "+:", "#[", "+", "-", "*", "/", "%", "~", "!", "&", "|", "^", "<", ">", "=", ".",
    ",", ";", ":", "@", "#", "(", ")", "[", "]", "{", "}", "?",
];

pub fn lex(file: FileId, text: &str, diags: &mut Diagnostics) -> Vec<Token> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let span = |s: usize, e: usize| Span::new(file, s, e);
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if text[i..].starts_with("//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if text[i..].starts_with("/*") {
            let start = i;
            match text[i + 2..].find("*/") {
                Some(p) => i += p + 4,
                None => {
                    diags.push(
                        Diagnostic::error(span(start, start + 2), "unterminated block comment")
                            .with_label("this comment never ends")
                            .with_help("close it with `*/`"),
                    );
                    i = b.len();
                }
            }
            continue;
        }
        let start = i;
        if c.is_ascii_alphabetic() || c == b'_' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Token { tok: Tok::Ident(text[start..i].to_string()), span: span(start, i) });
            continue;
        }
        if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'?') {
                i += 1;
            }
            let lit = &text[start..i];
            // Verilog sized literal: 8'hFF.
            if i < b.len() && b[i] == b'\'' {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                diags.push(
                    Diagnostic::error(span(start, j), "Verilog-style sized literals are not supported")
                        .with_label("sized literal")
                        .with_help("write the value alone (`0xFF`, `0b1010`, `42`): its width comes from where it is used")
                        .with_help("to give a constant an explicit type, convert it: `uint<8>(0xFF)`"),
                );
                out.push(Token { tok: Tok::Int(BigInt::zero(), None), span: span(start, j) });
                i = j;
                continue;
            }
            match number(lit) {
                Ok(t) => out.push(Token { tok: t, span: span(start, i) }),
                Err(msg) => {
                    diags.push(Diagnostic::error(span(start, i), msg).with_label("invalid number"));
                    out.push(Token { tok: Tok::Int(BigInt::zero(), None), span: span(start, i) });
                }
            }
            continue;
        }
        if c == b'"' {
            i += 1;
            let mut s = String::new();
            let mut closed = false;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        closed = true;
                        i += 1;
                        break;
                    }
                    b'\\' if i + 1 < b.len() => {
                        s.push(match b[i + 1] {
                            b'n' => '\n',
                            b't' => '\t',
                            other => other as char,
                        });
                        i += 2;
                    }
                    b'\n' => break,
                    _ => {
                        let ch = text[i..].chars().next().unwrap();
                        s.push(ch);
                        i += ch.len_utf8();
                    }
                }
            }
            if !closed {
                diags.push(
                    Diagnostic::error(span(start, i), "unterminated string")
                        .with_label("this string never ends")
                        .with_help("close it with `\"` on the same line"),
                );
            }
            out.push(Token { tok: Tok::Str(s), span: span(start, i) });
            continue;
        }
        if let Some(p) = PUNCTS.iter().find(|p| text[i..].starts_with(**p)) {
            i += p.len();
            out.push(Token { tok: Tok::Punct(p), span: span(start, i) });
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        let mut d =
            Diagnostic::error(span(i, i + ch.len_utf8()), format!("unexpected character `{ch}`"))
                .with_label("not valid here");
        if ch == '$' {
            d = d.with_help("Weft has no system tasks: `$clog2(n)` is `clog2(n)`, `$signed(x)` is `x.as_sint()`, `$readmemh` is `init hex_file(\"...\")`");
        } else if ch == '`' {
            d = d.with_help("Weft has no preprocessor: use `const` for values and `if` on a constant for conditional code");
        }
        diags.push(d);
        i += ch.len_utf8();
    }
    out.push(Token { tok: Tok::Eof, span: span(b.len(), b.len()) });
    out
}

fn number(lit: &str) -> Result<Tok, String> {
    let (radix, digits) = if let Some(d) = lit.strip_prefix("0x") {
        (16, d)
    } else if let Some(d) = lit.strip_prefix("0b") {
        (2, d)
    } else if let Some(d) = lit.strip_prefix("0o") {
        (8, d)
    } else {
        (10, lit)
    };
    if digits.contains('?') {
        if radix != 2 {
            return Err("`?` digits are only allowed in binary bit patterns (`0b1??0`)".into());
        }
        let ds: Vec<char> = digits.chars().filter(|&c| c != '_').collect();
        if ds.is_empty() || ds.len() > 127 {
            return Err("bit patterns must have between 1 and 127 digits".into());
        }
        let (mut value, mut care) = (0u128, 0u128);
        for c in &ds {
            value <<= 1;
            care <<= 1;
            match c {
                '0' => care |= 1,
                '1' => {
                    care |= 1;
                    value |= 1
                }
                '?' => {}
                _ => return Err(format!("invalid binary digit `{c}`")),
            }
        }
        return Ok(Tok::Bits(value, care, ds.len() as u32));
    }
    let v = BigInt::parse(digits, radix).ok_or_else(|| {
        let name = match radix {
            16 => "hexadecimal",
            2 => "binary",
            8 => "octal",
            _ => "decimal",
        };
        format!("invalid {name} number `{lit}`")
    })?;
    let ndigits = digits.chars().filter(|&c| c != '_').count() as u32;
    let natural = match radix {
        2 => Some(ndigits),
        16 => Some(ndigits * 4),
        _ => None,
    };
    Ok(Tok::Int(v, natural))
}
