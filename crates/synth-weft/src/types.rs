//! Types, integer ranges and timing tags.

use synth_core::{BigInt, Span};

/// A declared integer type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntDecl {
    Uint(u32),
    Sint(u32),
    Index(BigInt),
}

impl IntDecl {
    pub fn range(&self) -> (BigInt, BigInt) {
        match self {
            IntDecl::Uint(n) => (BigInt::zero(), &BigInt::pow2(*n as u64) - &BigInt::from_i64(1)),
            IntDecl::Sint(n) => {
                let h = BigInt::pow2(*n as u64 - 1);
                (-&h, &h - &BigInt::from_i64(1))
            }
            IntDecl::Index(n) => (BigInt::zero(), n - &BigInt::from_i64(1)),
        }
    }

    pub fn name(&self) -> String {
        match self {
            IntDecl::Uint(n) => format!("uint<{n}>"),
            IntDecl::Sint(n) => format!("sint<{n}>"),
            IntDecl::Index(n) => format!("index<{n}>"),
        }
    }
}

/// An integer type: a range of values, and the declared type it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntTy {
    pub lo: BigInt,
    pub hi: BigInt,
    pub decl: Option<IntDecl>,
}

impl IntTy {
    pub fn decl(d: IntDecl) -> IntTy {
        let (lo, hi) = d.range();
        IntTy { lo, hi, decl: Some(d) }
    }

    pub fn range(lo: BigInt, hi: BigInt) -> IntTy {
        IntTy { lo, hi, decl: None }
    }

    pub fn konst(v: &BigInt) -> IntTy {
        IntTy::range(v.clone(), v.clone())
    }

    pub fn signed(&self) -> bool {
        self.lo.is_negative()
    }

    pub fn width(&self) -> u32 {
        if let Some(IntDecl::Index(n)) = &self.decl {
            // The representation of an index does not shrink when narrowed.
            let w = bits_unsigned(&(n - &BigInt::from_i64(1)));
            return w.max(natural_width(&self.lo, &self.hi));
        }
        natural_width(&self.lo, &self.hi)
    }

    pub fn contains(&self, o: &IntTy) -> bool {
        self.lo <= o.lo && o.hi <= self.hi
    }

    pub fn contains_value(&self, v: &BigInt) -> bool {
        &self.lo <= v && v <= &self.hi
    }

    /// The declared type if the range is the declared one.
    pub fn full_decl(&self) -> Option<&IntDecl> {
        let d = self.decl.as_ref()?;
        let (lo, hi) = d.range();
        (lo == self.lo && hi == self.hi).then_some(d)
    }

    pub fn range_text(&self) -> String {
        if self.lo == self.hi {
            format!("{}", self.lo)
        } else {
            format!("{}..={}", self.lo, self.hi)
        }
    }

    /// The representation as a declared `uint`/`sint`.
    pub fn natural_decl(&self) -> IntDecl {
        if self.signed() { IntDecl::Sint(self.width()) } else { IntDecl::Uint(self.width()) }
    }
}

/// Bits needed for `0..=v` as unsigned (at least 1).
pub fn bits_unsigned(v: &BigInt) -> u32 {
    (v.bit_len() as u32).max(1)
}

/// Bits needed for `v` in two's complement.
fn bits_signed(v: &BigInt) -> u32 {
    if v.is_negative() {
        let m = &(-v) - &BigInt::from_i64(1);
        m.bit_len() as u32 + 1
    } else {
        v.bit_len() as u32 + 1
    }
}

/// Width of the natural representation of `lo..=hi`.
pub fn natural_width(lo: &BigInt, hi: &BigInt) -> u32 {
    if lo.is_negative() { bits_signed(lo).max(bits_signed(hi)) } else { bits_unsigned(hi) }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ty {
    Bool,
    Int(IntTy),
    Enum(usize),
    Struct(usize),
    Array(Box<Ty>, u64),
}

#[derive(Clone, Debug)]
pub struct EnumInfo {
    pub name: String,
    pub variants: Vec<(String, BigInt)>,
    pub width: u32,
    pub explicit: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct StructInfo {
    pub name: String,
    pub fields: Vec<(String, Ty)>,
}

/// Enum and struct definitions, shared by the whole design.
#[derive(Clone, Debug, Default)]
pub struct TypeDefs {
    pub enums: Vec<EnumInfo>,
    pub structs: Vec<StructInfo>,
}

impl TypeDefs {
    pub fn width(&self, t: &Ty) -> u32 {
        match t {
            Ty::Bool => 1,
            Ty::Int(i) => i.width(),
            Ty::Enum(e) => self.enums[*e].width,
            Ty::Struct(s) => self.structs[*s].fields.iter().map(|(_, f)| self.width(f)).sum(),
            Ty::Array(e, n) => self.width(e) * *n as u32,
        }
    }

    pub fn name(&self, t: &Ty) -> String {
        match t {
            Ty::Bool => "bool".into(),
            Ty::Int(i) => match i.full_decl() {
                Some(d) => d.name(),
                None => format!("int<{}..={}>", i.lo, i.hi),
            },
            Ty::Enum(e) => self.enums[*e].name.clone(),
            Ty::Struct(s) => self.structs[*s].name.clone(),
            Ty::Array(e, n) => format!("[{}; {n}]", self.name(e)),
        }
    }

    /// Whether two types are the same apart from integer ranges.
    pub fn same_shape(&self, a: &Ty, b: &Ty) -> bool {
        match (a, b) {
            (Ty::Bool, Ty::Bool) => true,
            (Ty::Int(_), Ty::Int(_)) => true,
            (Ty::Enum(x), Ty::Enum(y)) | (Ty::Struct(x), Ty::Struct(y)) => x == y,
            (Ty::Array(x, n), Ty::Array(y, m)) => n == m && self.same_shape(x, y),
            _ => false,
        }
    }

    /// Whether `from_bits` may build this type (no enums, no indices).
    pub fn can_build_from_bits(&self, t: &Ty) -> bool {
        match t {
            Ty::Bool => true,
            Ty::Int(i) => !matches!(i.decl, Some(IntDecl::Index(_))),
            Ty::Enum(_) => false,
            Ty::Struct(s) => {
                self.structs[*s].fields.iter().all(|(_, f)| self.can_build_from_bits(f))
            }
            Ty::Array(e, _) => self.can_build_from_bits(e),
        }
    }
}

/// A clock identity, for tag checking.
pub type ClockId = u32;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TagAtom {
    Clock(ClockId),
    Async,
    /// The (not yet known) tag of a wire.
    Var(u32),
}

/// The timing tag of a value: the join of its atoms (empty is static).
/// Each atom remembers where it first entered the value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags(pub Vec<(TagAtom, Origin)>);

/// Where a tag atom came from, for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub span: Span,
    pub what: String,
}

impl Tags {
    pub fn none() -> Tags {
        Tags(Vec::new())
    }

    pub fn one(a: TagAtom, span: Span, what: impl Into<String>) -> Tags {
        Tags(vec![(a, Origin { span, what: what.into() })])
    }

    pub fn is_static(&self) -> bool {
        self.0.is_empty()
    }

    pub fn join(&mut self, o: &Tags) {
        for (a, org) in &o.0 {
            if !self.0.iter().any(|(b, _)| b == a) {
                self.0.push((*a, org.clone()));
            }
        }
    }

    pub fn joined(mut self, o: &Tags) -> Tags {
        self.join(o);
        self
    }
}
