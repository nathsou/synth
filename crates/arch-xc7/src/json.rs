//! Strict, dependency-free JSON reader for the upstream device database.
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool,
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Result<&Self, String> {
        self.object()?.get(key).ok_or_else(|| format!("missing JSON field {key}"))
    }
    pub fn object(&self) -> Result<&BTreeMap<String, Self>, String> {
        if let Self::Object(v) = self { Ok(v) } else { Err("expected JSON object".into()) }
    }
    pub fn array(&self) -> Result<&[Self], String> {
        if let Self::Array(v) = self { Ok(v) } else { Err("expected JSON array".into()) }
    }
    pub fn string(&self) -> Result<&str, String> {
        if let Self::String(v) = self { Ok(v) } else { Err("expected JSON string".into()) }
    }
    pub fn integer(&self) -> Result<i64, String> {
        match self {
            Self::Number(v) | Self::String(v) => {
                v.parse().map_err(|_| format!("invalid integer {v}"))
            }
            _ => Err("expected JSON integer".into()),
        }
    }
}

pub fn parse(s: &str) -> Result<Value, String> {
    let mut p = Parser { bytes: s.as_bytes(), pos: 0 };
    let v = p.value(0)?;
    p.space();
    if p.pos != p.bytes.len() {
        return p.error("trailing data");
    }
    Ok(v)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl Parser<'_> {
    fn error<T>(&self, msg: &str) -> Result<T, String> {
        Err(format!("JSON byte {}: {msg}", self.pos))
    }
    fn space(&mut self) {
        while self.bytes.get(self.pos).is_some_and(|c| matches!(c, b' ' | b'\n' | b'\t' | b'\r')) {
            self.pos += 1;
        }
    }
    fn take(&mut self, c: u8) -> bool {
        self.space();
        if self.bytes.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.take(c) { Ok(()) } else { self.error(&format!("expected '{}'", c as char)) }
    }
    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > 128 {
            return self.error("nesting limit");
        }
        self.space();
        match self.bytes.get(self.pos).copied() {
            Some(b'{') => {
                self.pos += 1;
                let mut m = BTreeMap::new();
                if !self.take(b'}') {
                    loop {
                        let k = self.string()?;
                        self.expect(b':')?;
                        if m.insert(k, self.value(depth + 1)?).is_some() {
                            return self.error("duplicate object key");
                        }
                        if self.take(b'}') {
                            break;
                        }
                        self.expect(b',')?;
                    }
                }
                Ok(Value::Object(m))
            }
            Some(b'[') => {
                self.pos += 1;
                let mut v = Vec::new();
                if !self.take(b']') {
                    loop {
                        v.push(self.value(depth + 1)?);
                        if self.take(b']') {
                            break;
                        }
                        self.expect(b',')?;
                    }
                }
                Ok(Value::Array(v))
            }
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b'n' | b't' | b'f') => {
                let word: &[u8] = match self.bytes[self.pos] {
                    b'n' => b"null",
                    b't' => b"true",
                    _ => b"false",
                };
                if !self.bytes[self.pos..].starts_with(word) {
                    return self.error("invalid literal");
                }
                self.pos += word.len();
                Ok(if word == b"null" { Value::Null } else { Value::Bool })
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.pos;
                if self.bytes[self.pos] == b'-' {
                    self.pos += 1;
                }
                if self.bytes.get(self.pos) == Some(&b'0') {
                    self.pos += 1;
                } else {
                    self.digits()?;
                }
                if self.bytes.get(self.pos) == Some(&b'.') {
                    self.pos += 1;
                    self.digits()?;
                }
                if self.bytes.get(self.pos).is_some_and(|c| matches!(c, b'e' | b'E')) {
                    self.pos += 1;
                    if self.bytes.get(self.pos).is_some_and(|c| matches!(c, b'+' | b'-')) {
                        self.pos += 1;
                    }
                    self.digits()?;
                }
                Ok(Value::Number(std::str::from_utf8(&self.bytes[start..self.pos]).unwrap().into()))
            }
            _ => self.error("expected value"),
        }
    }
    fn digits(&mut self) -> Result<(), String> {
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        if self.pos == start { self.error("expected digit") } else { Ok(()) }
    }
    fn hex4(&mut self) -> Result<u32, String> {
        let mut n = 0;
        for _ in 0..4 {
            let c = *self.bytes.get(self.pos).ok_or("truncated Unicode escape")?;
            self.pos += 1;
            n = n * 16 + (c as char).to_digit(16).ok_or("invalid Unicode escape")?;
        }
        Ok(n)
    }
    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let c = *self.bytes.get(self.pos).ok_or("unterminated JSON string")?;
            self.pos += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|_| "invalid UTF-8".into()),
                0..=31 => return self.error("control character in string"),
                b'\\' => {
                    let e = *self.bytes.get(self.pos).ok_or("truncated escape")?;
                    self.pos += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(10),
                        b'r' => out.push(13),
                        b't' => out.push(9),
                        b'u' => {
                            let mut n = self.hex4()?;
                            if (0xd800..=0xdbff).contains(&n) {
                                if !self.bytes[self.pos..].starts_with(b"\\u") {
                                    return self.error("missing low surrogate");
                                }
                                self.pos += 2;
                                let low = self.hex4()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    return self.error("invalid low surrogate");
                                }
                                n = 0x10000 + ((n - 0xd800) << 10) + low - 0xdc00;
                            }
                            let ch = char::from_u32(n).ok_or("invalid Unicode code point")?;
                            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                        }
                        _ => return self.error("invalid escape"),
                    }
                }
                _ => out.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_json() {
        assert_eq!(
            parse(r#"{"key": "\ud83d\ude00"}"#).unwrap().get("key").unwrap().string().unwrap(),
            "😀"
        );
        for bad in ["01", "1.", "1e", "[1,]", "{\"x\":1,\"x\":2}", r#""\ud800""#, "null false"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
