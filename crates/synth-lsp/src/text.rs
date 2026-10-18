//! Conversions between byte offsets and LSP positions.

use synth_core::Json;

/// Line starts of a text, for position conversions.
#[derive(Debug)]
pub struct LineIndex<'a> {
    text: &'a str,
    starts: Vec<usize>,
    /// Columns count UTF-8 bytes instead of UTF-16 code units.
    utf8: bool,
}

impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str, utf8: bool) -> LineIndex<'a> {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { text, starts, utf8 }
    }

    pub fn position(&self, offset: usize) -> Json {
        let offset = offset.min(self.text.len());
        let line = self.starts.partition_point(|&s| s <= offset) - 1;
        let start = self.starts[line];
        let mut end = offset;
        while !self.text.is_char_boundary(end) {
            end -= 1;
        }
        let col =
            if self.utf8 { end - start } else { self.text[start..end].encode_utf16().count() };
        Json::obj().with("line", line).with("character", col)
    }

    pub fn range(&self, start: usize, end: usize) -> Json {
        Json::obj().with("start", self.position(start)).with("end", self.position(end))
    }

    /// The byte offset of an LSP position (clamped to the text).
    pub fn offset(&self, pos: &Json) -> usize {
        let line = pos.at(&["line"]).as_i64().unwrap_or(0).max(0) as usize;
        let col = pos.at(&["character"]).as_i64().unwrap_or(0).max(0) as usize;
        let Some(&start) = self.starts.get(line) else { return self.text.len() };
        let end = self.starts.get(line + 1).map_or(self.text.len(), |&e| e);
        let line_text = &self.text[start..end];
        if self.utf8 {
            let mut o = col.min(line_text.len());
            while !line_text.is_char_boundary(o) {
                o -= 1;
            }
            return start + o;
        }
        let mut units = 0;
        for (i, c) in line_text.char_indices() {
            if units >= col || c == '\n' {
                return start + i;
            }
            units += c.len_utf16();
        }
        start + line_text.trim_end_matches('\n').len()
    }
}

/// The path of a `file://` URI.
pub fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(h) = rest.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(h, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    let mut p = String::from_utf8(out).ok()?;
    // `file:///C:/x` on Windows.
    if p.len() > 2 && p.as_bytes()[0] == b'/' && p.as_bytes()[2] == b':' {
        p.remove(0);
    }
    Some(p)
}

pub fn path_to_uri(path: &str) -> String {
    let mut out = String::from("file://");
    if !path.starts_with('/') {
        out.push('/');
    }
    for b in path.replace('\\', "/").bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions() {
        let t = "ab\né😀x\n";
        let ix = LineIndex::new(t, false);
        let o = t.find('x').unwrap();
        let p = ix.position(o);
        assert_eq!(p.dump(), r#"{"line":1,"character":3}"#);
        assert_eq!(ix.offset(&p), o);
        let ix8 = LineIndex::new(t, true);
        assert_eq!(ix8.position(o).dump(), r#"{"line":1,"character":6}"#);
        assert_eq!(ix.offset(&Json::parse(r#"{"line":0,"character":99}"#).unwrap()), 2);
    }

    #[test]
    fn uris() {
        assert_eq!(uri_to_path("file:///a%20b/c.weft").as_deref(), Some("/a b/c.weft"));
        assert_eq!(path_to_uri("/a b/c.weft"), "file:///a%20b/c.weft");
    }
}
