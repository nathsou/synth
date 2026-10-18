//! String interning.
//!
//! Names (identifiers, cell types, hierarchical path segments) are interned to
//! a 4-byte [`Sym`] so they compare and hash in O(1) and are stored once.

use std::fmt;

use crate::define_id;
use crate::hash::FxHashMap;

define_id!(
    /// An interned string. Only meaningful together with its [`Interner`].
    pub struct Sym
);

/// Owns the interned strings and maps them to [`Sym`]s.
///
/// Symbols are allocated densely in first-seen order, so the mapping is
/// deterministic for a deterministic sequence of `intern` calls.
#[derive(Default, Clone)]
pub struct Interner {
    // Strings are stored back to back in `buf`; `spans[sym]` is (start, end).
    buf: String,
    spans: Vec<(u32, u32)>,
    lookup: FxHashMap<Box<str>, Sym>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Interns `s`, returning the existing symbol if it was seen before.
    pub fn intern(&mut self, s: &str) -> Sym {
        if let Some(&sym) = self.lookup.get(s) {
            return sym;
        }
        let start = u32::try_from(self.buf.len()).expect("interner overflow");
        self.buf.push_str(s);
        let end = u32::try_from(self.buf.len()).expect("interner overflow");
        let sym = Sym::new(self.spans.len());
        self.spans.push((start, end));
        self.lookup.insert(s.into(), sym);
        sym
    }

    /// Looks up `s` without interning it.
    pub fn get(&self, s: &str) -> Option<Sym> {
        self.lookup.get(s).copied()
    }

    /// Returns the string for `sym`.
    #[inline]
    pub fn resolve(&self, sym: Sym) -> &str {
        let (start, end) = self.spans[sym.index()];
        &self.buf[start as usize..end as usize]
    }

    /// Number of distinct strings interned.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

impl fmt::Debug for Interner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interner").field("len", &self.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_roundtrip() {
        let mut i = Interner::new();
        let a = i.intern("clk");
        let b = i.intern("rst");
        assert_eq!(i.intern("clk"), a);
        assert_ne!(a, b);
        assert_eq!(i.resolve(a), "clk");
        assert_eq!(i.resolve(b), "rst");
        assert_eq!(i.get("rst"), Some(b));
        assert_eq!(i.get("nope"), None);
        assert_eq!(i.intern(""), Sym::new(2));
        assert_eq!(i.resolve(Sym::new(2)), "");
    }
}
