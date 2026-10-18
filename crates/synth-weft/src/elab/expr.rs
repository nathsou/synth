//! Expression evaluation.

use std::collections::HashMap;

use synth_core::{BigInt, Diagnostic, Logic, LogicVec, Span};
use synth_rtl::{BinaryOp, MemRead, Sig, SigBit, UnaryOp};

use super::index::code;
use super::*;
use crate::ast::{Arg, BinOp, Expr, ExprKind, UnOp};

pub(crate) fn one() -> BigInt {
    BigInt::from_i64(1)
}

/// A constant integer as a hardware value of its natural width.
pub(crate) fn konst(v: &BigInt) -> Hw {
    let ty = IntTy::konst(v);
    Hw {
        sig: Sig::from_const(&v.to_logic_vec(ty.width() as usize)),
        ty: Ty::Int(ty),
        tags: Tags::none(),
    }
}

pub(crate) fn bool_hw(b: SigBit, tags: Tags) -> Hw {
    Hw { sig: Sig::bit(b), ty: Ty::Bool, tags }
}

pub(crate) fn const_bool(b: bool) -> Hw {
    bool_hw(if b { SigBit::ONE } else { SigBit::ZERO }, Tags::none())
}

/// Re-represents an integer signal for a (wider or equal, or narrowed) range.
pub(crate) fn adjust(sig: &Sig, from: &IntTy, to: &IntTy) -> Sig {
    let w = to.width() as usize;
    if w >= sig.width() { sig.extend(w, from.signed()) } else { sig.slice(0, w) }
}

fn int_of(h: &Hw) -> Option<&IntTy> {
    match &h.ty {
        Ty::Int(i) => Some(i),
        _ => None,
    }
}

/// The value of a constant integer signal.
pub(crate) fn const_int(h: &Hw) -> Option<BigInt> {
    let i = int_of(h)?;
    let c = h.sig.as_const()?;
    BigInt::from_logic_vec(&c, i.signed())
}

fn hull(a: &IntTy, b: &IntTy) -> IntTy {
    let lo = if a.lo < b.lo { a.lo.clone() } else { b.lo.clone() };
    let hi = if a.hi > b.hi { a.hi.clone() } else { b.hi.clone() };
    let decl = if a.decl == b.decl { a.decl.clone() } else { None };
    IntTy { lo, hi, decl }
}

impl<'a> ModCx<'a> {
    pub fn lookup(&mut self, d: &mut Design<'a>, id: &ast::Ident) -> R<Option<Sym>> {
        for s in self.scopes.iter().rev() {
            if let Some(sym) = s.syms.get(&id.name) {
                let sym = sym.clone();
                self.let_used.insert(id.name.clone());
                return Ok(Some(sym));
            }
        }
        if self.lazy.contains_key(&id.name) {
            self.let_used.insert(id.name.clone());
            return self.resolve_lazy(d, &id.name).map(Some);
        }
        if let Some(v) = self.generics.get(&id.name) {
            return Ok(Some(Sym::Value(v.clone())));
        }
        if let Some(c) = d.consts.get(id.name.as_str()).copied() {
            return self.global_const(d, c).map(|v| Some(Sym::Value(v)));
        }
        Ok(None)
    }

    fn global_const(&mut self, d: &mut Design<'a>, c: &'a ast::ConstDecl) -> R<Value> {
        if let Some(v) = d.const_cache.get(&c.name.name) {
            return match v {
                Some(v) => Ok(v.clone()),
                None => d.err(
                    Diagnostic::error(
                        c.name.span,
                        format!("constant `{}` depends on itself", c.name.name),
                    )
                    .with_label("cyclic definition"),
                ),
            };
        }
        d.const_cache.insert(c.name.name.clone(), None);
        let mut cx = ModCx::global();
        let v = cx.const_decl_value(d, c);
        match v {
            Ok(v) => {
                d.const_cache.insert(c.name.name.clone(), Some(v.clone()));
                Ok(v)
            }
            Err(()) => Err(()),
        }
    }

    pub fn const_decl_value(&mut self, d: &mut Design<'a>, c: &'a ast::ConstDecl) -> R<Value> {
        match &c.ty {
            Some(t) => {
                let ty = self.eval_type(d, t)?;
                let v = self.eval(d, &c.value, Some(&ty))?;
                let h = self.coerce(
                    d,
                    v,
                    &ty,
                    c.value.span,
                    &format!("constant `{}`", c.name.name),
                    Some(&c.value),
                )?;
                if !h.sig.is_const() || !h.tags.is_static() {
                    return d.err(
                        Diagnostic::error(
                            c.value.span,
                            "a constant's value must be known at compile time",
                        )
                        .with_label("depends on signals"),
                    );
                }
                Ok(Value::Hw(h))
            }
            None => {
                let v = self.eval_const(d, &c.value)?;
                match (c.kind, &v) {
                    (None, _)
                    | (Some((GenericKind::Int, _)), Value::Int(..))
                    | (Some((GenericKind::Str, _)), Value::Str(_)) => Ok(v),
                    (Some((k, sp)), _) => {
                        let what = if k == GenericKind::Int { "an integer" } else { "a string" };
                        d.err(
                            Diagnostic::error(
                                c.value.span,
                                format!("constant `{}` is declared {what}", c.name.name),
                            )
                            .with_label("a value of another kind")
                            .with_secondary(sp, "declared here"),
                        )
                    }
                }
            }
        }
    }

    /// The narrowed range of a name, if a comparison guards this point.
    fn narrowed(&self, name: &str) -> Option<&IntTy> {
        self.scopes.iter().rev().find_map(|s| s.narrow.get(name))
    }

    pub fn eval_type(&mut self, d: &mut Design<'a>, t: &'a ast::TypeExpr) -> R<Ty> {
        use ast::TypeKind::*;
        Ok(match &t.kind {
            Bool => Ty::Bool,
            Clock => {
                return d.err(
                    Diagnostic::error(t.span, "`clock` can only be the type of a clock input port")
                        .with_label("clock type here")
                        .with_help("clocks are not values: declare `in clk: clock` and refer to it in domains"),
                );
            }
            Uint(e) | Sint(e) => {
                let n = self.eval_int(d, e)?;
                let sint = matches!(t.kind, Sint(_));
                let Some(n) = n.to_u64().filter(|&n| (1..=1 << 20).contains(&n)) else {
                    return d.err(
                        Diagnostic::error(
                            e.span,
                            format!("width must be between 1 and 1048576, not {n}"),
                        )
                        .with_label("invalid width"),
                    );
                };
                Ty::Int(IntTy::decl(if sint {
                    IntDecl::Sint(n as u32)
                } else {
                    IntDecl::Uint(n as u32)
                }))
            }
            Index(e) => {
                let n = self.eval_int(d, e)?;
                if n < one() || n.bit_len() > 1 << 20 {
                    return d.err(
                        Diagnostic::error(
                            e.span,
                            format!("`index<{n}>` needs a size of at least 1"),
                        )
                        .with_label("invalid size"),
                    );
                }
                Ty::Int(IntTy::decl(IntDecl::Index(n)))
            }
            Array(e, n) => {
                let et = self.eval_type(d, e)?;
                let len = self.eval_int(d, n)?;
                let Some(len) = len.to_u64().filter(|&l| l >= 1) else {
                    return d.err(
                        Diagnostic::error(
                            n.span,
                            format!("array length must be at least 1, not {len}"),
                        )
                        .with_label("invalid length"),
                    );
                };
                Ty::Array(Box::new(et), len)
            }
            Named(n) => {
                if let Some(Value::Type(ty)) = self.generics.get(&n.name) {
                    let ty = ty.clone();
                    if d.indexing {
                        let hover = code(&format!("type {} = {}", n.name, d.types.name(&ty)));
                        d.note(
                            n.span,
                            self.generic_decls.get(&n.name).copied(),
                            RefKind::Generic,
                            hover,
                        );
                    }
                    return Ok(ty);
                }
                if let Some(ty) = d.type_names.get(&n.name) {
                    let ty = ty.clone();
                    if d.indexing {
                        d.note(n.span, d.type_decl(&ty), RefKind::Type, d.type_hover(&ty));
                        if let Ty::Enum(e) = &ty {
                            let vs =
                                d.types.enums[*e].variants.iter().map(|(n, _)| n.clone()).collect();
                            d.note_members(n.span, vs);
                        }
                    }
                    return Ok(ty);
                }
                let mut dg = Diagnostic::error(n.span, format!("unknown type `{}`", n.name))
                    .with_label("not a type");
                let cands: Vec<String> =
                    d.type_names.keys().cloned().chain(["bool".into()]).collect();
                if let Some(s) = suggest(&n.name, cands.iter().map(|s| s.as_str())) {
                    dg = dg.with_help(format!("did you mean `{s}`?"));
                } else if matches!(
                    n.name.as_str(),
                    "logic" | "reg" | "wire" | "bit" | "integer" | "u8" | "u16" | "u32" | "i32"
                ) {
                    dg = dg
                        .with_help("integer types are written `uint<N>`, `sint<N>` and `index<N>`");
                }
                return d.err(dg);
            }
        })
    }

    pub fn eval_int(&mut self, d: &mut Design<'a>, e: &'a Expr) -> R<BigInt> {
        match self.eval_const(d, e)? {
            Value::Int(v, _) => Ok(v),
            Value::Hw(h) if const_int(&h).is_some() => Ok(const_int(&h).unwrap()),
            _ => d.err(
                Diagnostic::error(e.span, "expected a constant integer")
                    .with_label("not an integer"),
            ),
        }
    }

    pub fn eval_const(&mut self, d: &mut Design<'a>, e: &'a Expr) -> R<Value> {
        let v = self.eval(d, e, None)?;
        let ok = match &v {
            Value::Hw(h) => {
                h.sig.is_const()
                    && h.tags.is_static()
                    && h.sig.iter().all(|b| !matches!(b, SigBit::Const(Logic::X | Logic::Z)))
            }
            Value::Int(..) | Value::Str(_) | Value::Type(_) => true,
            _ => false,
        };
        if ok {
            Ok(v)
        } else {
            d.err(
                Diagnostic::error(e.span, "this must be known at compile time")
                    .with_label("depends on signals")
                    .with_help("use constants, generic parameters or loop variables here"),
            )
        }
    }

    pub fn check_assert(&mut self, d: &mut Design<'a>, a: &'a ast::Assert) -> R<()> {
        let v = self.eval_const(d, &a.cond)?;
        match v {
            Value::Hw(h) if h.ty == Ty::Bool => {
                if h.sig.0[0] == SigBit::ZERO {
                    let mut dg =
                        Diagnostic::error(a.cond.span, format!("assertion failed: {}", a.message))
                            .with_label("this is false");
                    let gens: Vec<String> = {
                        let mut v: Vec<_> = self
                            .generics
                            .iter()
                            .filter_map(|(k, v)| match v {
                                Value::Int(i, _) => Some(format!("{k} = {i}")),
                                _ => None,
                            })
                            .collect();
                        v.sort();
                        v
                    };
                    if !gens.is_empty() {
                        dg = dg.with_note(None, format!("with {}", gens.join(", ")));
                    }
                    return d.err(dg);
                }
                Ok(())
            }
            _ => d.err(
                Diagnostic::error(a.cond.span, "an assertion needs a `bool` condition")
                    .with_label("not a bool"),
            ),
        }
    }

    /// A value as hardware (constants become their natural representation).
    #[allow(clippy::wrong_self_convention)]
    pub fn to_hw(&mut self, d: &mut Design<'a>, v: Value, span: Span) -> R<Hw> {
        match v {
            Value::Hw(h) => Ok(h),
            Value::Int(c, _) => Ok(konst(&c)),
            Value::Str(_) => d.err(Diagnostic::error(span, "a string is not a hardware value").with_label("string")),
            Value::Clock(_) | Value::Domain(_) => d.err(
                Diagnostic::error(span, "a clock cannot be used as a value")
                    .with_label("clock")
                    .with_help("clocks only drive registers; to react to a slow rate, use a clock enable (`if tick { ... }`)"),
            ),
            Value::Inst(i) => {
                let names: Vec<String> = self.insts[i].ports.iter().map(|(n, _)| format!("`.{n}`")).collect();
                d.err(
                    Diagnostic::error(span, format!("instance `{}` is not a value", self.insts[i].name))
                        .with_label("an instance")
                        .with_help(format!("read one of its ports: {}", names.join(", "))),
                )
            }
            Value::Mem(i) => d.err(
                Diagnostic::error(span, format!("memory `{}` cannot be used as a whole", self.mems[i].name))
                    .with_label("a memory")
                    .with_help("read one element: `mem[address]`"),
            ),
            Value::Type(_) => d.err(Diagnostic::error(span, "a type is not a value").with_label("a type")),
        }
    }

    pub fn eval_hw(&mut self, d: &mut Design<'a>, e: &'a Expr, exp: Option<&Ty>) -> R<Hw> {
        let v = self.eval(d, e, exp)?;
        self.to_hw(d, v, e.span)
    }

    /// Evaluates `e` and converts it to `ty` without loss.
    pub fn eval_to(&mut self, d: &mut Design<'a>, e: &'a Expr, ty: &Ty, what: &str) -> R<Hw> {
        let v = self.eval(d, e, Some(ty))?;
        self.coerce(d, v, ty, e.span, what, Some(e))
    }

    pub fn coerce(
        &mut self,
        d: &mut Design<'a>,
        v: Value,
        ty: &Ty,
        span: Span,
        what: &str,
        src: Option<&Expr>,
    ) -> R<Hw> {
        let tname = d.types.name(ty);
        let snip = d.snippet(span);
        match (v, ty) {
            (Value::Int(c, _), Ty::Int(t)) => {
                if !t.contains_value(&c) {
                    return d.err(
                        Diagnostic::error(span, format!("{c} does not fit {what} ({tname})"))
                            .with_label(format!("{tname} holds {}", t.range_text())),
                    );
                }
                Ok(Hw {
                    sig: Sig::from_const(&c.to_logic_vec(t.width() as usize)),
                    ty: ty.clone(),
                    tags: Tags::none(),
                })
            }
            (Value::Int(..), Ty::Bool) => d.err(
                Diagnostic::error(span, format!("expected `bool` for {what}, found an integer"))
                    .with_label("integer")
                    .with_help("write `true` or `false`; `bool` is not an integer"),
            ),
            (Value::Hw(h), _) => self.coerce_hw(d, h, ty, span, what, src),
            (v, _) => {
                let h = self.to_hw(d, v, span)?;
                let _ = snip;
                self.coerce_hw(d, h, ty, span, what, src)
            }
        }
    }

    fn coerce_hw(
        &mut self,
        d: &mut Design<'a>,
        h: Hw,
        ty: &Ty,
        span: Span,
        what: &str,
        src: Option<&Expr>,
    ) -> R<Hw> {
        let tname = d.types.name(ty);
        match (&h.ty, ty) {
            (Ty::Int(f), Ty::Int(t)) => {
                if t.contains(f) {
                    return Ok(Hw { sig: adjust(&h.sig, f, t), ty: ty.clone(), tags: h.tags });
                }
                let snip = d.snippet(span);
                let fname = d.types.name(&h.ty);
                let vals = if fname.starts_with("int<") {
                    format!("has values {}", f.range_text())
                } else {
                    format!("{fname} has values {}", f.range_text())
                };
                let label = if f.lo < t.lo && t.lo >= BigInt::zero() {
                    format!("may be negative: {vals}")
                } else {
                    vals
                };
                let mut dg =
                    Diagnostic::error(span, format!("`{snip}` does not fit {what} ({tname})"))
                        .with_label(label)
                        .with_note(None, format!("{tname} holds {}", t.range_text()));
                if let Some(e) = src
                    && let ExprKind::Binary(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul), a, b) =
                        &e.strip().kind
                {
                    let w = match op {
                        BinOp::Add => "+%",
                        BinOp::Sub => "-%",
                        _ => "*%",
                    };
                    dg = dg.with_help(format!(
                        "to wrap around, use `{w}`: `{} {w} {}`",
                        d.snippet(a.span),
                        d.snippet(b.span)
                    ));
                }
                if let Some(td) = t.full_decl() {
                    match td {
                        IntDecl::Uint(n) | IntDecl::Sint(n) if f.width() > *n => {
                            dg = dg.with_help(format!(
                                "to keep the low {n} bits, write `({snip}).trunc({n})`"
                            ));
                        }
                        _ => {}
                    }
                }
                if f.lo.is_negative() && !t.lo.is_negative() {
                    dg = dg.with_help(
                        "check the sign first (`if x >= 0 { ... }`), or use a signed target",
                    );
                }
                d.err(dg)
            }
            (Ty::Array(fe, n), Ty::Array(te, m)) if n == m && d.types.same_shape(fe, te) => {
                if fe == te {
                    return Ok(Hw { sig: h.sig, ty: ty.clone(), tags: h.tags });
                }
                let fw = d.types.width(fe) as usize;
                let mut out = Sig::new();
                for k in 0..*n as usize {
                    let el = Hw {
                        sig: h.sig.slice(k * fw, fw),
                        ty: (**fe).clone(),
                        tags: h.tags.clone(),
                    };
                    let c =
                        self.coerce_hw(d, el, te, span, &format!("element {k} of {what}"), None)?;
                    out = out.concat(&c.sig);
                }
                Ok(Hw { sig: out, ty: ty.clone(), tags: h.tags })
            }
            (a, b) if a == b => Ok(h),
            (Ty::Bool, Ty::Int(_)) => d.err(
                Diagnostic::error(span, format!("expected {tname} for {what}, found `bool`"))
                    .with_label("bool")
                    .with_help("`bool` is not an integer: convert it with `.bits()` (a uint<1>)"),
            ),
            (Ty::Int(_), Ty::Bool) => d.err(
                Diagnostic::error(
                    span,
                    format!("expected `bool` for {what}, found {}", d.types.name(&h.ty)),
                )
                .with_label("integer")
                .with_help("integers are not conditions: compare explicitly, for example `x != 0`"),
            ),
            (Ty::Enum(_), Ty::Int(_)) | (Ty::Struct(_), Ty::Int(_)) => d.err(
                Diagnostic::error(
                    span,
                    format!("expected {tname} for {what}, found {}", d.types.name(&h.ty)),
                )
                .with_label("not an integer")
                .with_help("`.bits()` gives the bit representation as a uint"),
            ),
            _ => d.err(
                Diagnostic::error(
                    span,
                    format!("expected {tname} for {what}, found {}", d.types.name(&h.ty)),
                )
                .with_label(format!("this is {}", d.types.name(&h.ty))),
            ),
        }
    }

    /// The common type of two branches.
    fn join_types(&self, _d: &Design<'a>, a: &Ty, b: &Ty) -> Option<Ty> {
        join_types(a, b)
    }

    pub fn eval(&mut self, d: &mut Design<'a>, e: &'a Expr, exp: Option<&Ty>) -> R<Value> {
        let reg_read = std::mem::replace(&mut self.reg_read, false);
        let span = e.span;
        match &e.kind {
            ExprKind::Int(v, w) => Ok(Value::Int(v.clone(), *w)),
            ExprKind::Bool(b) => Ok(Value::Hw(const_bool(*b))),
            ExprKind::Str(s) => Ok(Value::Str(s.clone())),
            ExprKind::Paren(x) => {
                self.reg_read = reg_read;
                self.eval(d, x, exp)
            }
            ExprKind::Undef => match exp {
                Some(t) => {
                    let w = d.types.width(t) as usize;
                    Ok(Value::Hw(Hw { sig: Sig::xs(w), ty: t.clone(), tags: Tags::none() }))
                }
                None => d
                    .err(
                        Diagnostic::error(span, "the type of `undef` is not known here")
                            .with_label("needs a type")
                            .with_help("use `undef` where a type is expected, such as an assignment or an `if` branch"),
                    ),
            },
            ExprKind::Variant(v) => match exp {
                Some(Ty::Enum(ei)) => self.variant(d, *ei, v),
                Some(t) => {
                    let tn = d.types.name(t);
                    d.err(Diagnostic::error(span, format!("`.{}` is an enum variant, but {tn} is expected here", v.name)).with_label("variant"))
                }
                None => d
                    .err(
                        Diagnostic::error(span, format!("the enum of `.{}` is not known here", v.name))
                            .with_label("which enum?")
                            .with_help(format!("name the enum: `State.{}`", v.name)),
                    ),
            },
            ExprKind::Ident(id) => self.eval_ident(d, id),
            ExprKind::Default => match self.default_domain(d, span)? {
                Some(i) => Ok(Value::Domain(i)),
                None => d.err(self.no_default_error(d, span, "`default`")),
            },
            ExprKind::Field(base, name) => self.eval_field(d, base, name, span),
            ExprKind::Method(base, name, args) => self.eval_method(d, base, name, args, span),
            ExprKind::Index(base, idx) => self.eval_index(d, base, idx, span, reg_read),
            ExprKind::Slice(base, hi, lo) => {
                let b = self.eval_hw(d, base, None)?;
                let hi_v = self.eval_int(d, hi)?;
                let lo_v = self.eval_int(d, lo)?;
                let Some(bi) = self.bits_of(d, &b, base.span)? else { unreachable!() };
                let w = bi.width() as i64;
                let (Some(h), Some(l)) = (hi_v.to_i64(), lo_v.to_i64()) else { return d.err(Diagnostic::error(span, "slice bounds out of range").with_label("bad bounds")) };
                if l > h {
                    return d.err(
                        Diagnostic::error(span, format!("slices are written high to low: `[{h}:{l}]` has {h} < {l}"))
                            .with_label("reversed slice")
                            .with_help(format!("write `[{l}:{h}]`")),
                    );
                }
                if l < 0 || h >= w {
                    return d.err(
                        Diagnostic::error(span, format!("bits {h}..{l} are out of range: `{}` has {w} bits", d.snippet(base.span)))
                            .with_label(format!("valid bits are {}..0", w - 1)),
                    );
                }
                let n = (h - l + 1) as u32;
                Ok(Value::Hw(Hw { sig: b.sig.slice(l as usize, n as usize), ty: Ty::Int(IntTy::decl(IntDecl::Uint(n))), tags: b.tags }))
            }
            ExprKind::PartSelect(base, start, width) => {
                let b = self.eval_hw(d, base, None)?;
                let Some(bi) = self.bits_of(d, &b, base.span)? else { unreachable!() };
                let w = self.eval_int(d, width)?;
                let Some(w) = w.to_u64().filter(|&w| w >= 1 && w <= bi.width() as u64) else {
                    return d.err(Diagnostic::error(width.span, format!("width must be between 1 and {}", bi.width())).with_label("bad width"));
                };
                let st = self.eval_hw(d, start, None)?;
                let Some(si) = int_of(&st).cloned().filter(|i| !i.signed()) else {
                    return d.err(Diagnostic::error(start.span, "the start of a part select must be an unsigned integer").with_label("not unsigned"));
                };
                let max = BigInt::from_u64(bi.width() as u64 - w);
                if si.hi > max {
                    return d.err(
                        Diagnostic::error(start.span, format!("`{}` may be up to {}, but bits {}..{} would then be past the end of a {}-bit value", d.snippet(start.span), si.hi, &si.hi + &BigInt::from_u64(w - 1), si.hi, bi.width()))
                            .with_label(format!("must be at most {max}"))
                            .with_help(format!("guard it: `if {} <= {max} {{ ... }}`", d.snippet(start.span))),
                    );
                }
                let sig = d.out.shiftx(b.sig.clone(), st.sig.clone(), false, 0, w as usize);
                Ok(Value::Hw(Hw { sig, ty: Ty::Int(IntTy::decl(IntDecl::Uint(w as u32))), tags: b.tags.joined(&st.tags) }))
            }
            ExprKind::Call { callee, generics, args, dom } => self.eval_call(d, callee, generics, args, dom.as_ref(), span, exp, reg_read),
            ExprKind::Convert(t, x) => {
                let ty = self.eval_type(d, t)?;
                let v = self.eval(d, x, Some(&ty))?;
                let tn = d.types.name(&ty);
                self.coerce(d, v, &ty, x.span, &format!("`{tn}`"), Some(x)).map(Value::Hw)
            }
            ExprKind::Unary(op, x) => self.eval_unary(d, *op, x, span),
            ExprKind::Binary(op, a, b) => self.eval_binary(d, *op, a, b, span, exp),
            ExprKind::Block(b) => {
                self.reg_read = reg_read;
                self.eval_block(d, b, exp)
            }
            ExprKind::If(c, t, els) => self.eval_if(d, c, t, els, exp, reg_read),
            ExprKind::Match(s, arms) => self.eval_match_expr(d, s, arms, span, exp, reg_read),
            ExprKind::Struct(name, fields) => self.eval_struct(d, name, fields, span),
            ExprKind::Array(items) => {
                let et = match exp {
                    Some(Ty::Array(et, n)) if *n == items.len() as u64 => Some((**et).clone()),
                    _ => None,
                };
                let mut hs = Vec::new();
                for (k, it) in items.iter().enumerate() {
                    hs.push(match &et {
                        Some(t) => self.eval_to(d, it, t, &format!("array element {k}"))?,
                        None => self.eval_hw(d, it, None)?,
                    });
                }
                let ty = match et {
                    Some(t) => t,
                    None => {
                        let mut t = hs[0].ty.clone();
                        for (k, h) in hs.iter().enumerate().skip(1) {
                            match self.join_types(d, &t, &h.ty) {
                                Some(j) => t = j,
                                None => {
                                    let (a, b) = (d.types.name(&t), d.types.name(&h.ty));
                                    return d.err(Diagnostic::error(items[k].span, format!("array elements have different types: {a} and {b}")).with_label(b));
                                }
                            }
                        }
                        t
                    }
                };
                let mut sig = Sig::new();
                let mut tags = Tags::none();
                for h in hs {
                    let c = self.coerce_hw(d, h, &ty, span, "an array element", None)?;
                    tags.join(&c.tags);
                    sig = sig.concat(&c.sig);
                }
                Ok(Value::Hw(Hw { sig, ty: Ty::Array(Box::new(ty), items.len() as u64), tags }))
            }
            ExprKind::ArrayRepeat(v, n) => {
                let len = self.eval_int(d, n)?;
                let Some(len) = len.to_u64().filter(|&l| (1..=1 << 20).contains(&l)) else {
                    return d.err(Diagnostic::error(n.span, "array length must be between 1 and 1048576").with_label("bad length"));
                };
                let et = match exp {
                    Some(Ty::Array(et, _)) => Some((**et).clone()),
                    _ => None,
                };
                let h = match &et {
                    Some(t) => self.eval_to(d, v, t, "the array element")?,
                    None => self.eval_hw(d, v, None)?,
                };
                let mut sig = Sig::new();
                for _ in 0..len {
                    sig = sig.concat(&h.sig);
                }
                Ok(Value::Hw(Hw { sig, ty: Ty::Array(Box::new(h.ty), len), tags: h.tags }))
            }
        }
    }

    fn variant(&mut self, d: &mut Design<'a>, ei: usize, v: &ast::Ident) -> R<Value> {
        let info = &d.types.enums[ei];
        match info.variants.iter().find(|(n, _)| *n == v.name) {
            Some((_, val)) => {
                let sig = Sig::from_const(&val.to_logic_vec(info.width as usize));
                if d.indexing {
                    let hover = code(&format!("{}.{} = {val}", info.name, v.name));
                    let decl = d.enum_defs[ei]
                        .variants
                        .iter()
                        .find(|(n, _)| n.name == v.name)
                        .map(|(n, _)| n.span);
                    d.note(v.span, decl, RefKind::Variant, hover);
                }
                Ok(Value::Hw(Hw { sig, ty: Ty::Enum(ei), tags: Tags::none() }))
            }
            None => {
                let name = info.name.clone();
                let mut dg =
                    Diagnostic::error(v.span, format!("`{name}` has no variant `{}`", v.name))
                        .with_label("unknown variant");
                if let Some(s) = suggest(&v.name, info.variants.iter().map(|(n, _)| n.as_str())) {
                    dg = dg.with_help(format!("did you mean `.{s}`?"));
                } else {
                    let vs: Vec<String> =
                        info.variants.iter().map(|(n, _)| format!("`.{n}`")).collect();
                    dg = dg.with_help(format!("its variants are {}", vs.join(", ")));
                }
                dg = dg.with_secondary(info.span, format!("`{name}` is defined here"));
                d.err(dg)
            }
        }
    }

    pub fn eval_ident(&mut self, d: &mut Design<'a>, id: &ast::Ident) -> R<Value> {
        let sym = self.lookup(d, id)?;
        let Some(sym) = sym else {
            if let Some(t) = d.type_names.get(&id.name) {
                return Ok(Value::Type(t.clone()));
            }
            let mut cands: Vec<String> = Vec::new();
            for s in &self.scopes {
                cands.extend(s.syms.keys().cloned());
            }
            cands.extend(self.lazy.keys().cloned());
            cands.extend(self.generics.keys().cloned());
            cands.extend(d.consts.keys().cloned());
            let mut dg = Diagnostic::error(id.span, format!("`{}` is not declared", id.name))
                .with_label("not found in this scope");
            if let Some(s) = suggest(&id.name, cands.iter().map(|s| s.as_str())) {
                dg = dg.with_help(format!("did you mean `{s}`?"));
            } else if self.mode == Mode::Module {
                dg = dg.with_help(format!(
                    "declare it, for example `wire {}: bool;` or `let {} = ...;`",
                    id.name, id.name
                ));
            }
            return d.err(dg);
        };
        self.note_sym(d, id, &sym);
        Ok(match sym {
            Sym::Value(v) => v,
            Sym::Var(h) => {
                if let Ty::Int(i) = &h.ty
                    && let Some(n) = self.narrowed(&id.name)
                    && n != i
                {
                    let n = n.clone();
                    return Ok(Value::Hw(Hw {
                        sig: adjust(&h.sig, i, &n),
                        ty: Ty::Int(n),
                        tags: h.tags,
                    }));
                }
                Value::Hw(h)
            }
            Sym::Signal(i) => {
                self.signals[i].read = true;
                let s = &self.signals[i];
                let tags = match &s.tag {
                    Some(FixedTag { atom: Some(a), what }) => {
                        let what = format!("`{}` ({what})", s.name);
                        Tags::one(*a, id.span, what)
                    }
                    Some(FixedTag { atom: None, .. }) => Tags::none(),
                    None => Tags::one(TagAtom::Var(i as u32), id.span, format!("`{}`", s.name)),
                };
                let mut h = Hw { sig: s.sig.clone(), ty: s.ty.clone(), tags };
                if let Ty::Int(it) = &h.ty
                    && let Some(n) = self.narrowed(&id.name)
                {
                    let n = n.clone();
                    h.sig = adjust(&h.sig, it, &n);
                    h.ty = Ty::Int(n);
                }
                Value::Hw(h)
            }
            Sym::Domain(i) | Sym::ClockPort(i) => Value::Domain(i),
            Sym::Inst(i) => Value::Inst(i),
            Sym::Mem(i) => Value::Mem(i),
        })
    }

    fn eval_field(
        &mut self,
        d: &mut Design<'a>,
        base: &'a Expr,
        name: &ast::Ident,
        span: Span,
    ) -> R<Value> {
        let bv = self.eval(d, base, None)?;
        match bv {
            Value::Type(t) => {
                if name.name == "width" {
                    return Ok(Value::Int(BigInt::from_u64(d.types.width(&t) as u64), None));
                }
                if let Ty::Enum(ei) = t {
                    return self.variant(d, ei, name);
                }
                d.err(
                    Diagnostic::error(
                        name.span,
                        format!("type {} has no `.{}`", d.types.name(&t), name.name),
                    )
                    .with_label("unknown"),
                )
            }
            Value::Domain(i) => match name.name.as_str() {
                "clock" => Ok(Value::Clock(self.domains[i].clock.clone())),
                "reset" => {
                    let dm = &self.domains[i];
                    let bit = dm.reset.map(|(b, _)| b).unwrap_or(SigBit::ZERO);
                    let tags = Tags::one(
                        TagAtom::Clock(dm.clock.id),
                        span,
                        format!("the reset of domain `{}`", dm.name),
                    );
                    Ok(Value::Hw(bool_hw(bit, tags)))
                }
                _ => d.err(
                    Diagnostic::error(name.span, format!("a domain has no `.{}`", name.name))
                        .with_label("unknown")
                        .with_help("domains have `.clock` and `.reset`"),
                ),
            },
            Value::Inst(i) => {
                if let Some((_, c)) = self.insts[i].clocks.iter().find(|(n, _)| *n == name.name) {
                    return Ok(Value::Clock(c.clone()));
                }
                let Some(&(_, si)) = self.insts[i].ports.iter().find(|(n, _)| *n == name.name)
                else {
                    let inst = self.insts[i].clone();
                    let mut dg = Diagnostic::error(
                        name.span,
                        format!(
                            "instance `{}` of `{}` has no port `{}`",
                            inst.name, inst.module, name.name
                        ),
                    )
                    .with_label("unknown port");
                    if let Some(s) = suggest(&name.name, inst.ports.iter().map(|(n, _)| n.as_str()))
                    {
                        dg = dg.with_help(format!("did you mean `{}.{s}`?", inst.name));
                    } else {
                        let ps: Vec<String> =
                            inst.ports.iter().map(|(n, _)| format!("`{n}`")).collect();
                        dg = dg.with_help(format!("its ports are {}", ps.join(", ")));
                    }
                    return d.err(dg);
                };
                self.signals[si].read = true;
                if d.indexing {
                    let module = self.insts[i].module.clone();
                    let decl = d.port_of(&module, &name.name).map(|p| p.name.span);
                    let hover = self.describe_signal(d, si)
                        + &format!("\n\nport of instance `{}` of `{module}`", self.insts[i].name);
                    d.note(name.span, decl, RefKind::Port, hover);
                }
                let s = &self.signals[si];
                let tags = match &s.tag {
                    Some(FixedTag { atom: Some(a), what }) => Tags::one(
                        *a,
                        span,
                        format!("`{}.{}` ({what})", self.insts[i].name, name.name),
                    ),
                    _ => Tags::none(),
                };
                Ok(Value::Hw(Hw { sig: s.sig.clone(), ty: s.ty.clone(), tags }))
            }
            Value::Hw(h) => {
                if name.name == "width" {
                    return Ok(Value::Int(BigInt::from_u64(d.types.width(&h.ty) as u64), None));
                }
                let Ty::Struct(si) = h.ty else {
                    let tn = d.types.name(&h.ty);
                    return d.err(
                        Diagnostic::error(name.span, format!("{tn} has no field `{}`", name.name))
                            .with_label("no such field")
                            .with_help("only structs have fields; methods are called with `()`"),
                    );
                };
                let (off, w, ty) = match self.field_layout(d, si, &name.name) {
                    Some(x) => {
                        if d.indexing {
                            let decl = d.struct_defs[si]
                                .fields
                                .iter()
                                .find(|(n, _)| n.name == name.name)
                                .map(|(n, _)| n.span);
                            let hover = code(&format!("{}: {}", name.name, d.types.name(&x.2)))
                                + &format!(
                                    "\n\nfield of `{}`, {}",
                                    d.types.structs[si].name,
                                    super::index::bits(x.1)
                                );
                            d.note(name.span, decl, RefKind::Field, hover);
                        }
                        x
                    }
                    None => {
                        let info = &d.types.structs[si];
                        let mut dg = Diagnostic::error(
                            name.span,
                            format!("`{}` has no field `{}`", info.name, name.name),
                        )
                        .with_label("unknown field");
                        if let Some(s) =
                            suggest(&name.name, info.fields.iter().map(|(n, _)| n.as_str()))
                        {
                            dg = dg.with_help(format!("did you mean `.{s}`?"));
                        }
                        return d.err(dg);
                    }
                };
                Ok(Value::Hw(Hw { sig: h.sig.slice(off as usize, w as usize), ty, tags: h.tags }))
            }
            Value::Clock(_) => d.err(
                Diagnostic::error(name.span, format!("a clock has no `.{}`", name.name))
                    .with_label("unknown"),
            ),
            other => {
                let _ = other;
                d.err(
                    Diagnostic::error(
                        name.span,
                        format!("`{}` has no field `{}`", d.snippet(base.span), name.name),
                    )
                    .with_label("unknown field"),
                )
            }
        }
    }

    /// Bit offset, width and type of a struct field (first field highest).
    pub fn field_layout(&self, d: &Design<'a>, si: usize, name: &str) -> Option<(u32, u32, Ty)> {
        let info = &d.types.structs[si];
        let mut off = 0;
        for (n, t) in info.fields.iter().rev() {
            let w = d.types.width(t);
            if n == name {
                return Some((off, w, t.clone()));
            }
            off += w;
        }
        None
    }

    /// An integer or bool value as raw bits (for slicing).
    fn bits_of(&mut self, d: &mut Design<'a>, h: &Hw, span: Span) -> R<Option<IntTy>> {
        match &h.ty {
            Ty::Int(i) => Ok(Some(i.clone())),
            t => {
                let tn = d.types.name(t);
                d.err(
                    Diagnostic::error(span, format!("cannot take bits of {tn}"))
                        .with_label("not an integer")
                        .with_help("convert it with `.bits()` first"),
                )
            }
        }
    }

    fn eval_method(
        &mut self,
        d: &mut Design<'a>,
        base: &'a Expr,
        name: &ast::Ident,
        args: &'a [Arg],
        span: Span,
    ) -> R<Value> {
        let m = name.name.as_str();
        let bv = self.eval(d, base, None)?;
        if let Value::Int(c, _) = &bv
            && m != "trunc"
        {
            return d.err(
                Diagnostic::error(
                    span,
                    format!("`.{m}()` needs a value with a width, and {c} has none"),
                )
                .with_label("unsized constant")
                .with_help(format!("give it a type first: `uint<8>({c}).{m}()`")),
            );
        }
        let h = self.to_hw(d, bv, base.span)?;
        let nargs = |n: usize, d: &mut Design<'a>| -> R<()> {
            if args.len() != n {
                return d.err(
                    Diagnostic::error(
                        span,
                        format!(
                            "`.{m}()` takes {n} argument{}, not {}",
                            if n == 1 { "" } else { "s" },
                            args.len()
                        ),
                    )
                    .with_label("wrong number of arguments"),
                );
            }
            Ok(())
        };
        let uint = |n: u32| Ty::Int(IntTy::decl(IntDecl::Uint(n)));
        let need_unsigned = |s: &mut Self, d: &mut Design<'a>| -> R<IntTy> {
            match &h.ty {
                Ty::Int(i) if !i.signed() => Ok(i.clone()),
                Ty::Int(_) => d.err(
                    Diagnostic::error(base.span, format!("`.{m}()` works on unsigned values"))
                        .with_label("signed")
                        .with_help("take the raw bits first: `x.bits()`"),
                ),
                t => {
                    let tn = d.types.name(t);
                    let _ = s;
                    d.err(
                        Diagnostic::error(
                            base.span,
                            format!("`.{m}()` works on unsigned integers, not {tn}"),
                        )
                        .with_label(tn)
                        .with_help("convert with `.bits()` first"),
                    )
                }
            }
        };
        match m {
            "bits" => {
                nargs(0, d)?;
                if let Ty::Enum(ei) = h.ty
                    && !d.types.enums[ei].explicit
                {
                    let en = d.types.enums[ei].name.clone();
                    return d.err(
                        Diagnostic::error(span, format!("`{en}` has no bit representation"))
                            .with_label("encoding chosen by the compiler")
                            .with_help(format!("give it an explicit encoding (`enum {en}: uint<N> {{ A = 0, ... }}`), or `match` on it"))
                            .with_secondary(d.types.enums[ei].span, "declared here"),
                    );
                }
                let w = h.sig.width() as u32;
                Ok(Value::Hw(Hw { sig: h.sig, ty: uint(w), tags: h.tags }))
            }
            "as_sint" => {
                nargs(0, d)?;
                let i = need_unsigned(self, d)?;
                let w = i.width();
                Ok(Value::Hw(Hw {
                    sig: h.sig,
                    ty: Ty::Int(IntTy::decl(IntDecl::Sint(w))),
                    tags: h.tags,
                }))
            }
            "trunc" => {
                nargs(1, d)?;
                let n = self.eval_int(d, &args[0].value)?;
                let Ty::Int(i) = &h.ty else {
                    let tn = d.types.name(&h.ty);
                    return d.err(
                        Diagnostic::error(
                            base.span,
                            format!("`.trunc()` works on integers, not {tn}"),
                        )
                        .with_label(tn),
                    );
                };
                let w = i.width();
                let Some(n) = n.to_u64().filter(|&n| n >= 1 && n <= w as u64) else {
                    return d.err(
                        Diagnostic::error(
                            args[0].value.span,
                            format!("cannot keep {n} bits of a {w}-bit value"),
                        )
                        .with_label(format!("must be between 1 and {w}")),
                    );
                };
                let ty = if i.signed() { IntDecl::Sint(n as u32) } else { IntDecl::Uint(n as u32) };
                Ok(Value::Hw(Hw {
                    sig: h.sig.slice(0, n as usize),
                    ty: Ty::Int(IntTy::decl(ty)),
                    tags: h.tags,
                }))
            }
            "any" | "all" | "parity" => {
                nargs(0, d)?;
                need_unsigned(self, d)?;
                let op = match m {
                    "any" => UnaryOp::ReduceOr,
                    "all" => UnaryOp::ReduceAnd,
                    _ => UnaryOp::ReduceXor,
                };
                let b = d.out.unary(op, h.sig).0[0];
                Ok(Value::Hw(bool_hw(b, h.tags)))
            }
            "reverse" => {
                nargs(0, d)?;
                let i = need_unsigned(self, d)?;
                let sig: Sig = h.sig.iter().rev().copied().collect();
                Ok(Value::Hw(Hw { sig, ty: uint(i.width()), tags: h.tags }))
            }
            "replicate" => {
                nargs(1, d)?;
                let i = need_unsigned(self, d)?;
                let k = self.eval_int(d, &args[0].value)?;
                let Some(k) = k.to_u64().filter(|&k| k >= 1 && k * i.width() as u64 <= 1 << 20)
                else {
                    return d.err(
                        Diagnostic::error(args[0].value.span, "the count must be at least 1")
                            .with_label("bad count"),
                    );
                };
                let mut sig = Sig::new();
                for _ in 0..k {
                    sig = sig.concat(&h.sig);
                }
                Ok(Value::Hw(Hw { sig, ty: uint(i.width() * k as u32), tags: h.tags }))
            }
            "rotate_left" | "rotate_right" => {
                nargs(1, d)?;
                let i = need_unsigned(self, d)?;
                let w = i.width() as usize;
                let amt = self.eval(d, &args[0].value, None)?;
                match amt {
                    Value::Int(n, _) => {
                        let Some(n) = n.to_u64() else {
                            return d.err(
                                Diagnostic::error(
                                    args[0].value.span,
                                    "rotation amount must not be negative",
                                )
                                .with_label("negative"),
                            );
                        };
                        let n = (n % w as u64) as usize;
                        let k = if m == "rotate_left" { (w - n) % w } else { n };
                        let sig: Sig = (0..w).map(|j| h.sig.0[(j + k) % w]).collect();
                        Ok(Value::Hw(Hw { sig, ty: uint(w as u32), tags: h.tags }))
                    }
                    other => {
                        let a = self.to_hw(d, other, args[0].value.span)?;
                        if !matches!(&a.ty, Ty::Int(x) if !x.signed()) {
                            return d.err(
                                Diagnostic::error(
                                    args[0].value.span,
                                    "rotation amount must be unsigned",
                                )
                                .with_label("not unsigned"),
                            );
                        }
                        // Rotate as a shift of the doubled value.
                        let doubled = h.sig.concat(&h.sig);
                        let amount = if m == "rotate_left" {
                            let wsig = Sig::from_u64(
                                w as u64,
                                a.sig
                                    .width()
                                    .max(bits_unsigned(&BigInt::from_u64(w as u64)) as usize),
                            );
                            let ext = a.sig.extend(wsig.width(), false);
                            let modded = d.out.binary(
                                BinaryOp::Mod,
                                false,
                                ext,
                                Sig::from_u64(w as u64, wsig.width()),
                            );
                            d.out.binary(BinaryOp::Sub, false, wsig, modded)
                        } else {
                            let wd = a
                                .sig
                                .width()
                                .max(bits_unsigned(&BigInt::from_u64(w as u64)) as usize);
                            d.out.binary(
                                BinaryOp::Mod,
                                false,
                                a.sig.extend(wd, false),
                                Sig::from_u64(w as u64, wd),
                            )
                        };
                        let sig = d.out.shiftx(doubled, amount, false, 0, w);
                        Ok(Value::Hw(Hw { sig, ty: uint(w as u32), tags: h.tags.joined(&a.tags) }))
                    }
                }
            }
            _ => {
                let all = [
                    "bits",
                    "as_sint",
                    "trunc",
                    "any",
                    "all",
                    "parity",
                    "reverse",
                    "replicate",
                    "rotate_left",
                    "rotate_right",
                ];
                let mut dg = Diagnostic::error(name.span, format!("unknown method `.{m}()`"))
                    .with_label("no such method");
                if let Some(s) = suggest(m, all) {
                    dg = dg.with_help(format!("did you mean `.{s}()`?"));
                } else {
                    dg = dg.with_help(format!(
                        "methods are {}",
                        all.map(|x| format!("`.{x}()`")).join(", ")
                    ));
                }
                d.err(dg)
            }
        }
    }

    /// Checks that a runtime index stays within `0..len`.
    fn check_index(
        &mut self,
        d: &mut Design<'a>,
        idx: &Hw,
        idx_span: Span,
        len: u64,
        what: &str,
    ) -> R<()> {
        let Ty::Int(i) = &idx.ty else {
            let tn = d.types.name(&idx.ty);
            return d.err(
                Diagnostic::error(idx_span, format!("an index must be an integer, not {tn}"))
                    .with_label(tn.clone())
                    .with_help(if tn == "bool" {
                        "use `.bits()` for a uint<1>".to_string()
                    } else {
                        "use an integer value".to_string()
                    }),
            );
        };
        let max = BigInt::from_u64(len - 1);
        if i.lo.is_negative() || i.hi > max {
            let snip = d.snippet(idx_span);
            let mut dg = Diagnostic::error(
                idx_span,
                format!("index `{snip}` may be out of range: {what} has {len} entries"),
            )
            .with_label(format!("values {}, but valid indices are 0..={max}", i.range_text()));
            if !i.lo.is_negative() {
                dg = dg
                    .with_help(format!("use an `index<{len}>` value, which is always in range"))
                    .with_help(format!("or guard the access: `if {snip} < {len} {{ ... }}`"));
            } else {
                dg = dg.with_help(
                    "indices are unsigned: check the sign first, or use an unsigned value",
                );
            }
            return d.err(dg);
        }
        Ok(())
    }

    fn eval_index(
        &mut self,
        d: &mut Design<'a>,
        base: &'a Expr,
        idx: &'a Expr,
        span: Span,
        reg_read: bool,
    ) -> R<Value> {
        let bv = self.eval(d, base, None)?;
        let iv = self.eval(d, idx, None)?;
        match bv {
            Value::Mem(mi) => {
                let depth = self.mems[mi].depth;
                let ih = self.to_hw(d, iv, idx.span)?;
                self.check_index(
                    d,
                    &ih,
                    idx.span,
                    depth,
                    &format!("memory `{}`", self.mems[mi].name),
                )?;
                let aw = bits_unsigned(&BigInt::from_u64(depth - 1)) as usize;
                let addr = ih.sig.extend(aw, false).slice(0, aw);
                let elem = self.mems[mi].elem.clone();
                let w = d.types.width(&elem);
                let data = d.add_wire(
                    &format!("{}$rd{}", self.name_of(&self.mems[mi].name), self.uniq),
                    w,
                    span,
                    false,
                );
                self.uniq += 1;
                let id = self.mems[mi].id;
                d.out.memories[id].reads.push(MemRead {
                    clk: None,
                    en: SigBit::ONE,
                    addr,
                    data: data.clone(),
                    transparent: vec![],
                });
                self.mems[mi].read = true;
                if !reg_read {
                    self.mems[mi].comb_reads.push(span);
                }
                let dm = &self.domains[self.mems[mi].dom];
                let tags = ih.tags.joined(&Tags::one(
                    TagAtom::Clock(dm.clock.id),
                    span,
                    format!("memory `{}` (domain `{}`)", self.mems[mi].name, dm.name),
                ));
                Ok(Value::Hw(Hw { sig: data, ty: elem, tags }))
            }
            Value::Hw(h) => {
                match &h.ty {
                    Ty::Array(et, n) => {
                        let ew = d.types.width(et) as usize;
                        let n = *n;
                        match iv {
                            Value::Int(c, _) => {
                                let Some(k) = c.to_u64().filter(|&k| k < n) else {
                                    return d.err(
                                    Diagnostic::error(idx.span, format!("index {c} is out of range: the array has {n} elements"))
                                        .with_label(format!("valid indices are 0..={}", n - 1)),
                                );
                                };
                                Ok(Value::Hw(Hw {
                                    sig: h.sig.slice(k as usize * ew, ew),
                                    ty: (**et).clone(),
                                    tags: h.tags,
                                }))
                            }
                            other => {
                                let ih = self.to_hw(d, other, idx.span)?;
                                self.check_index(
                                    d,
                                    &ih,
                                    idx.span,
                                    n,
                                    &format!("`{}`", d.snippet(base.span)),
                                )?;
                                if let Some(k) = const_int(&ih).and_then(|k| k.to_u64()) {
                                    return Ok(Value::Hw(Hw {
                                        sig: h.sig.slice(k as usize * ew, ew),
                                        ty: (**et).clone(),
                                        tags: h.tags.joined(&ih.tags),
                                    }));
                                }
                                let sig = self.select_element(d, &h.sig, ew, n, &ih);
                                Ok(Value::Hw(Hw {
                                    sig,
                                    ty: (**et).clone(),
                                    tags: h.tags.joined(&ih.tags),
                                }))
                            }
                        }
                    }
                    Ty::Int(it) => {
                        let w = it.width() as u64;
                        match iv {
                            Value::Int(c, _) => {
                                let Some(k) = c.to_u64().filter(|&k| k < w) else {
                                    return d.err(
                                        Diagnostic::error(
                                            idx.span,
                                            format!(
                                                "bit {c} is out of range: `{}` has {w} bits",
                                                d.snippet(base.span)
                                            ),
                                        )
                                        .with_label(format!("valid bits are 0..={}", w - 1)),
                                    );
                                };
                                Ok(Value::Hw(bool_hw(h.sig.0[k as usize], h.tags)))
                            }
                            other => {
                                let ih = self.to_hw(d, other, idx.span)?;
                                self.check_index(
                                    d,
                                    &ih,
                                    idx.span,
                                    w,
                                    &format!("`{}` ({w} bits)", d.snippet(base.span)),
                                )?;
                                let b =
                                    d.out.shiftx(h.sig.clone(), ih.sig.clone(), false, 0, 1).0[0];
                                Ok(Value::Hw(bool_hw(b, h.tags.joined(&ih.tags))))
                            }
                        }
                    }
                    t => {
                        let tn = d.types.name(t);
                        d.err(
                            Diagnostic::error(base.span, format!("cannot index {tn}"))
                                .with_label(tn)
                                .with_help(
                                    "arrays, memories and integers (their bits) can be indexed",
                                ),
                        )
                    }
                }
            }
            other => {
                let _ = self.to_hw(d, other, base.span)?;
                unreachable!()
            }
        }
    }

    /// `elements[i]` for a runtime index known to be in range.
    pub fn select_element(
        &mut self,
        d: &mut Design<'a>,
        sig: &Sig,
        ew: usize,
        n: u64,
        idx: &Hw,
    ) -> Sig {
        let Ty::Int(it) = &idx.ty else { unreachable!() };
        let mut cases = Vec::new();
        let mut sel = Sig::new();
        let lo = it.lo.to_u64().unwrap_or(0);
        let hi = it.hi.to_u64().unwrap_or(n - 1).min(n - 1);
        for k in lo..=hi {
            cases.push(sig.slice(k as usize * ew, ew));
            let c = BigInt::from_u64(k).to_logic_vec(idx.sig.width());
            sel.push(d.out.eq_const(idx.sig.clone(), &c));
        }
        d.out.pmux(Sig::xs(ew), cases, sel)
    }

    fn eval_unary(&mut self, d: &mut Design<'a>, op: UnOp, x: &'a Expr, span: Span) -> R<Value> {
        let v = self.eval(d, x, None)?;
        match op {
            UnOp::Neg => match v {
                Value::Int(c, _) => Ok(Value::Int(-&c, None)),
                other => {
                    let h = self.to_hw(d, other, x.span)?;
                    let Ty::Int(i) = &h.ty else {
                        let tn = d.types.name(&h.ty);
                        return d.err(
                            Diagnostic::error(span, format!("cannot negate {tn}")).with_label(tn),
                        );
                    };
                    let r = IntTy::range(-&i.hi, -&i.lo);
                    let w = r.width() as usize;
                    let sig = d.out.unary(UnaryOp::Neg, h.sig.extend(w, i.signed()));
                    Ok(Value::Hw(Hw { sig, ty: Ty::Int(r), tags: h.tags }))
                }
            },
            UnOp::Not => {
                let h = self.to_hw(d, v, x.span)?;
                if h.ty != Ty::Bool {
                    let tn = d.types.name(&h.ty);
                    let mut dg = Diagnostic::error(span, format!("`!` needs a `bool`, not {tn}"))
                        .with_label(tn);
                    if matches!(h.ty, Ty::Int(_)) {
                        dg = dg
                            .with_help(format!(
                                "for the bitwise complement, use `~{}`",
                                d.snippet(x.span)
                            ))
                            .with_help(format!(
                                "to test for zero, use `{} == 0`",
                                d.snippet(x.span)
                            ));
                    }
                    return d.err(dg);
                }
                let b = d.out.not1(h.sig.0[0]);
                Ok(Value::Hw(bool_hw(b, h.tags)))
            }
            UnOp::BitNot => {
                if let Value::Int(c, _) = &v {
                    return d.err(
                        Diagnostic::error(span, format!("`~{c}` needs a width"))
                            .with_label("unsized constant")
                            .with_help(format!("give it a type: `~uint<8>({c})`")),
                    );
                }
                let h = self.to_hw(d, v, x.span)?;
                match &h.ty {
                    Ty::Int(i) if !i.signed() => {
                        let w = i.width();
                        let sig = d.out.unary(UnaryOp::Not, h.sig);
                        Ok(Value::Hw(Hw {
                            sig,
                            ty: Ty::Int(IntTy::decl(IntDecl::Uint(w))),
                            tags: h.tags,
                        }))
                    }
                    Ty::Bool => d.err(
                        Diagnostic::error(
                            span,
                            "`~` is for integers; a `bool` is negated with `!`",
                        )
                        .with_label("bool")
                        .with_help(format!("write `!{}`", d.snippet(x.span))),
                    ),
                    t => {
                        let tn = d.types.name(t);
                        d.err(
                            Diagnostic::error(
                                span,
                                format!("`~` needs an unsigned integer, not {tn}"),
                            )
                            .with_label(tn)
                            .with_help("take the raw bits first: `~x.bits()`"),
                        )
                    }
                }
            }
        }
    }

    fn int_const_op(
        &mut self,
        d: &mut Design<'a>,
        op: BinOp,
        a: &BigInt,
        b: &BigInt,
        span: Span,
    ) -> R<Value> {
        use BinOp::*;
        let int = |v: BigInt| Ok(Value::Int(v, None));
        let boolv = |b: bool| Ok(Value::Hw(const_bool(b)));
        match op {
            Add => int(a + b),
            Sub => int(a - b),
            Mul => int(a * b),
            Div | Rem => match a.div_rem(b) {
                Some((q, r)) => int(if op == Div { q } else { r }),
                None => {
                    d.err(Diagnostic::error(span, "division by zero").with_label("divisor is 0"))
                }
            },
            Pow => match b.to_u64().filter(|&e| e <= 1 << 16) {
                Some(e) => int(a.pow(e)),
                None => d.err(
                    Diagnostic::error(span, "the exponent must be between 0 and 65536")
                        .with_label("bad exponent"),
                ),
            },
            Eq => boolv(a == b),
            Ne => boolv(a != b),
            Lt => boolv(a < b),
            Le => boolv(a <= b),
            Gt => boolv(a > b),
            Ge => boolv(a >= b),
            Shl | Shr => match b.to_u64().filter(|&n| n <= 1 << 20) {
                Some(n) => {
                    int(if op == Shl { a.shl(n) } else { a.div_floor(&BigInt::pow2(n)).unwrap() })
                }
                None => d.err(
                    Diagnostic::error(span, "shift amount out of range").with_label("bad amount"),
                ),
            },
            AddWrap | SubWrap | MulWrap => d.err(
                Diagnostic::error(
                    span,
                    format!("`{}` needs a typed operand to wrap at", op.symbol()),
                )
                .with_label("both operands are constants")
                .with_help("constants are exact; use `+`, `-` or `*`"),
            ),
            And | Or | Xor => d.err(
                Diagnostic::error(
                    span,
                    format!("`{}` on two constants needs a width", op.symbol()),
                )
                .with_label("unsized constants")
                .with_help("give one a type: `uint<8>(a) & b`"),
            ),
            LogicAnd | LogicOr => d.err(
                Diagnostic::error(span, format!("`{}` needs `bool` operands", op.symbol()))
                    .with_label("integers"),
            ),
        }
    }

    fn eval_binary(
        &mut self,
        d: &mut Design<'a>,
        op: BinOp,
        a: &'a Expr,
        b: &'a Expr,
        span: Span,
        exp: Option<&Ty>,
    ) -> R<Value> {
        use BinOp::*;
        let _ = exp;
        // An enum variant on one side takes its type from the other.
        let (va, vb) = if matches!(a.strip().kind, ExprKind::Variant(_) | ExprKind::Undef) {
            let vb = self.eval(d, b, None)?;
            let t = match &vb {
                Value::Hw(h) => Some(h.ty.clone()),
                _ => None,
            };
            (self.eval(d, a, t.as_ref())?, vb)
        } else {
            let va = self.eval(d, a, None)?;
            let t = match &va {
                Value::Hw(h)
                    if matches!(b.strip().kind, ExprKind::Variant(_) | ExprKind::Undef) =>
                {
                    Some(h.ty.clone())
                }
                _ => None,
            };
            (va, self.eval(d, b, t.as_ref())?)
        };
        if let (Value::Int(x, _), Value::Int(y, _)) = (&va, &vb) {
            return self.int_const_op(d, op, x, y, span);
        }
        match op {
            LogicAnd | LogicOr => {
                let ha = self.to_hw(d, va, a.span)?;
                let hb = self.to_hw(d, vb, b.span)?;
                for (h, e) in [(&ha, a), (&hb, b)] {
                    if h.ty != Ty::Bool {
                        let tn = d.types.name(&h.ty);
                        let mut dg = Diagnostic::error(
                            e.span,
                            format!("`{}` needs `bool` operands, but this is {tn}", op.symbol()),
                        )
                        .with_label(tn);
                        if matches!(h.ty, Ty::Int(_)) {
                            dg = dg
                                .with_help(format!("compare it: `{} != 0`", d.snippet(e.span)))
                                .with_help(format!(
                                    "for a bitwise operation, use `{}`",
                                    if op == LogicAnd { "&" } else { "|" }
                                ));
                        }
                        return d.err(dg);
                    }
                }
                let bit = if op == LogicAnd {
                    d.out.and1(ha.sig.0[0], hb.sig.0[0])
                } else {
                    d.out.or1(ha.sig.0[0], hb.sig.0[0])
                };
                Ok(Value::Hw(bool_hw(bit, ha.tags.joined(&hb.tags))))
            }
            Eq | Ne | Lt | Le | Gt | Ge => self.compare(d, op, va, vb, a, b, span),
            And | Or | Xor => self.bitwise(d, op, va, vb, a, b, span),
            Shl | Shr => self.shift(d, op, va, vb, a, b, span),
            Pow => d.err(
                Diagnostic::error(span, "`**` is only for compile-time integers")
                    .with_label("operand is a signal")
                    .with_help("for squares, multiply: `x * x`"),
            ),
            AddWrap | SubWrap | MulWrap => self.wrap_arith(d, op, va, vb, a, b, span),
            Add | Sub | Mul | Div | Rem => self.exact_arith(d, op, va, vb, a, b, span),
        }
    }

    fn need_int(&mut self, d: &mut Design<'a>, v: Value, e: &'a Expr, op: BinOp) -> R<(Hw, IntTy)> {
        let h = self.to_hw(d, v, e.span)?;
        match &h.ty {
            Ty::Int(i) => {
                let i = i.clone();
                Ok((h, i))
            }
            t => {
                let tn = d.types.name(t);
                let mut dg = Diagnostic::error(
                    e.span,
                    format!("`{}` needs integer operands, but this is {tn}", op.symbol()),
                )
                .with_label(tn.clone());
                if matches!(t, Ty::Bool) {
                    dg = dg.with_help("`bool` is not an integer: use `.bits()` for a uint<1>");
                } else if matches!(t, Ty::Struct(_) | Ty::Enum(_) | Ty::Array(..)) {
                    dg = dg.with_help("use `.bits()` for its bit representation");
                }
                d.err(dg)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn exact_arith(
        &mut self,
        d: &mut Design<'a>,
        op: BinOp,
        va: Value,
        vb: Value,
        a: &'a Expr,
        b: &'a Expr,
        _span: Span,
    ) -> R<Value> {
        use BinOp::*;
        if matches!(op, Div | Rem) {
            let c = match &vb {
                Value::Int(c, _) => Some(c.clone()),
                Value::Hw(h) if h.tags.is_static() => const_int(h),
                _ => None,
            };
            let Some(c) = c.filter(|c| *c > BigInt::zero()) else {
                return d.err(
                    Diagnostic::error(b.span, format!("`{}` needs a constant positive divisor", op.symbol()))
                        .with_label(if matches!(vb, Value::Int(..)) { "not positive" } else { "not a constant" })
                        .with_help("dividers for runtime values are large and slow; for powers of two, use `>>` or a slice"),
                );
            };
            let (ha, ia) = self.need_int(d, va, a, op)?;
            let (lo, hi) = if op == Div {
                (ia.lo.div_rem(&c).unwrap().0, ia.hi.div_rem(&c).unwrap().0)
            } else if !ia.lo.is_negative() {
                let top = &c - &one();
                let hi = if ia.hi < top { ia.hi.clone() } else { top };
                (BigInt::zero(), hi)
            } else {
                let top = &c - &one();
                let lo_lim = -&top;
                let lo = if ia.lo > lo_lim { ia.lo.clone() } else { lo_lim };
                let hi = if ia.hi.is_negative() {
                    BigInt::zero()
                } else if ia.hi < top {
                    ia.hi.clone()
                } else {
                    top
                };
                (lo, hi)
            };
            let res = IntTy::range(lo, hi);
            let cw = c.bit_len() as u32 + 1;
            let w = (natural_width(&ia.lo, &ia.hi) + 1).max(cw).max(res.width()) as usize;
            let x = ha.sig.extend(w, ia.signed());
            let y = Sig::from_const(&c.to_logic_vec(w));
            let q = d.out.binary(if op == Div { BinaryOp::Div } else { BinaryOp::Mod }, true, x, y);
            return Ok(Value::Hw(Hw {
                sig: q.slice(0, res.width() as usize),
                ty: Ty::Int(res),
                tags: ha.tags,
            }));
        }
        let (ha, ia) = self.need_int(d, va, a, op)?;
        let (hb, ib) = self.need_int(d, vb, b, op)?;
        let (lo, hi) = match op {
            Add => (&ia.lo + &ib.lo, &ia.hi + &ib.hi),
            Sub => (&ia.lo - &ib.hi, &ia.hi - &ib.lo),
            _ => {
                let c = [&ia.lo * &ib.lo, &ia.lo * &ib.hi, &ia.hi * &ib.lo, &ia.hi * &ib.hi];
                (c.iter().min().unwrap().clone(), c.iter().max().unwrap().clone())
            }
        };
        let res = IntTy::range(lo, hi);
        let w = res.width() as usize;
        let signed = res.signed();
        let x = ha.sig.extend(w, ia.signed());
        let y = hb.sig.extend(w, ib.signed());
        let bop = match op {
            Add => BinaryOp::Add,
            Sub => BinaryOp::Sub,
            _ => BinaryOp::Mul,
        };
        let sig = d.out.binary(bop, signed, x, y);
        Ok(Value::Hw(Hw { sig, ty: Ty::Int(res), tags: ha.tags.joined(&hb.tags) }))
    }

    #[allow(clippy::too_many_arguments)]
    fn wrap_arith(
        &mut self,
        d: &mut Design<'a>,
        op: BinOp,
        va: Value,
        vb: Value,
        a: &'a Expr,
        b: &'a Expr,
        span: Span,
    ) -> R<Value> {
        let decl_of = |v: &Value| match v {
            Value::Hw(Hw { ty: Ty::Int(i), .. }) => i.decl.clone(),
            _ => None,
        };
        let (da, db) = (decl_of(&va), decl_of(&vb));
        let w = match (&da, &db) {
            (Some(x), Some(y)) if x != y => {
                return d.err(
                    Diagnostic::error(
                        span,
                        format!(
                            "`{}` operands have different types: {} and {}",
                            op.symbol(),
                            x.name(),
                            y.name()
                        ),
                    )
                    .with_label("which type should it wrap at?")
                    .with_help(format!(
                        "convert one operand, for example `{} {} {}({})`",
                        d.snippet(a.span),
                        op.symbol(),
                        x.name(),
                        d.snippet(b.span)
                    )),
                );
            }
            (Some(x), _) | (None, Some(x)) => x.clone(),
            (None, None) => {
                return d.err(
                    Diagnostic::error(
                        span,
                        format!(
                            "`{}` needs an operand with a declared type to wrap at",
                            op.symbol()
                        ),
                    )
                    .with_label("only ranges here")
                    .with_help(format!(
                        "give the value a type first: `uint<8>({})`",
                        d.snippet(a.span)
                    )),
                );
            }
        };
        let wt = Ty::Int(IntTy::decl(w.clone()));
        let what = format!("the `{}` (which wraps at {})", op.symbol(), w.name());
        let ha = self.coerce(d, va, &wt, a.span, &what, Some(a))?;
        let hb = self.coerce(d, vb, &wt, b.span, &what, Some(b))?;
        let tags = ha.tags.joined(&hb.tags);
        let width = d.types.width(&wt) as usize;
        let sig = match &w {
            IntDecl::Uint(_) | IntDecl::Sint(_) => {
                let signed = matches!(w, IntDecl::Sint(_));
                let bop = match op {
                    BinOp::AddWrap => BinaryOp::Add,
                    BinOp::SubWrap => BinaryOp::Sub,
                    _ => BinaryOp::Mul,
                };
                d.out.binary(bop, signed, ha.sig, hb.sig)
            }
            IntDecl::Index(n) => {
                if op == BinOp::MulWrap {
                    return d.err(
                        Diagnostic::error(span, format!("`*%` is not defined for {}", w.name()))
                            .with_label("index multiplication")
                            .with_help("only `+%` and `-%` wrap modulo the index size"),
                    );
                }
                let ww = width + 1;
                let x = ha.sig.extend(ww, false);
                let y = hb.sig.extend(ww, false);
                let nsig = Sig::from_const(&n.to_logic_vec(ww));
                if op == BinOp::AddWrap {
                    let s = d.out.binary(BinaryOp::Add, false, x, y);
                    let over = d.out.binary(BinaryOp::Ge, false, s.clone(), nsig.clone()).0[0];
                    let sub = d.out.binary(BinaryOp::Sub, false, s.clone(), nsig);
                    d.out.mux(over, s, sub).slice(0, width)
                } else {
                    let s = d.out.binary(BinaryOp::Sub, false, x.clone(), y.clone());
                    let under = d.out.binary(BinaryOp::Lt, false, x, y).0[0];
                    let add = d.out.binary(BinaryOp::Add, false, s.clone(), nsig);
                    d.out.mux(under, s, add).slice(0, width)
                }
            }
        };
        Ok(Value::Hw(Hw { sig, ty: wt, tags }))
    }

    #[allow(clippy::too_many_arguments)]
    fn compare(
        &mut self,
        d: &mut Design<'a>,
        op: BinOp,
        va: Value,
        vb: Value,
        a: &'a Expr,
        b: &'a Expr,
        span: Span,
    ) -> R<Value> {
        let ha = self.to_hw(d, va, a.span)?;
        let hb = self.to_hw(d, vb, b.span)?;
        let tags = ha.tags.clone().joined(&hb.tags);
        match (&ha.ty, &hb.ty) {
            (Ty::Int(ia), Ty::Int(ib)) => {
                // Statically decided comparisons are usually mistakes.
                let always = match op {
                    BinOp::Lt => {
                        (ia.hi < ib.lo).then_some(true).or((ia.lo >= ib.hi).then_some(false))
                    }
                    BinOp::Le => {
                        (ia.hi <= ib.lo).then_some(true).or((ia.lo > ib.hi).then_some(false))
                    }
                    BinOp::Gt => {
                        (ia.lo > ib.hi).then_some(true).or((ia.hi <= ib.lo).then_some(false))
                    }
                    BinOp::Ge => {
                        (ia.lo >= ib.hi).then_some(true).or((ia.hi < ib.lo).then_some(false))
                    }
                    BinOp::Eq => (ia.hi < ib.lo || ib.hi < ia.lo).then_some(false),
                    _ => (ia.hi < ib.lo || ib.hi < ia.lo).then_some(true),
                };
                let both_const = ia.lo == ia.hi && ib.lo == ib.hi;
                if let Some(v) = always
                    && !both_const
                {
                    let desc = |t: &IntTy, s: String| {
                        if t.lo == t.hi {
                            format!("{}", t.lo)
                        } else {
                            format!("`{s}` has values {}", t.range_text())
                        }
                    };
                    let (an, bn) = (desc(ia, d.snippet(a.span)), desc(ib, d.snippet(b.span)));
                    d.emit(
                        Diagnostic::warning(span, format!("this comparison is always {v}"))
                            .with_label(if ia.lo == ia.hi {
                                format!("{bn}, compared with {an}")
                            } else {
                                format!("{an}, compared with {bn}")
                            })
                            .with_help("check the constant or the operand's width"),
                    );
                }
                let lo = if ia.lo < ib.lo { ia.lo.clone() } else { ib.lo.clone() };
                let hi = if ia.hi > ib.hi { ia.hi.clone() } else { ib.hi.clone() };
                let common = IntTy::range(lo, hi);
                let w = common.width() as usize;
                let x = ha.sig.extend(w, ia.signed());
                let y = hb.sig.extend(w, ib.signed());
                let bop = match op {
                    BinOp::Eq => BinaryOp::Eq,
                    BinOp::Ne => BinaryOp::Ne,
                    BinOp::Lt => BinaryOp::Lt,
                    BinOp::Le => BinaryOp::Le,
                    BinOp::Gt => BinaryOp::Gt,
                    _ => BinaryOp::Ge,
                };
                let bit = d.out.binary(bop, common.signed(), x, y).0[0];
                Ok(Value::Hw(bool_hw(bit, tags)))
            }
            (ta, tb) if ta == tb || d.types.same_shape(ta, tb) && matches!(ta, Ty::Array(..)) => {
                if !matches!(op, BinOp::Eq | BinOp::Ne) {
                    let tn = d.types.name(ta);
                    return d.err(
                        Diagnostic::error(
                            span,
                            format!("`{}` orders integers, but these are {tn}", op.symbol()),
                        )
                        .with_label("not integers")
                        .with_help("only `==` and `!=` compare other types"),
                    );
                }
                let (x, y) = if ta == tb {
                    (ha.sig.clone(), hb.sig.clone())
                } else {
                    let joined = self.join_types(d, ta, tb).unwrap();
                    let x =
                        self.coerce_hw(d, ha.clone(), &joined, a.span, "the comparison", None)?.sig;
                    let y =
                        self.coerce_hw(d, hb.clone(), &joined, b.span, "the comparison", None)?.sig;
                    (x, y)
                };
                let bop = if op == BinOp::Eq { BinaryOp::Eq } else { BinaryOp::Ne };
                let bit = d.out.binary(bop, false, x, y).0[0];
                Ok(Value::Hw(bool_hw(bit, tags)))
            }
            (ta, tb) => {
                let (an, bn) = (d.types.name(ta), d.types.name(tb));
                let mut dg = Diagnostic::error(span, format!("cannot compare {an} with {bn}"))
                    .with_label("different types")
                    .with_secondary(a.span, an.clone())
                    .with_secondary(b.span, bn.clone());
                if (an == "bool") != (bn == "bool") {
                    dg = dg.with_help("`bool` is not an integer: compare a bool with `true`/`false` or use `.bits()`");
                }
                d.err(dg)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bitwise(
        &mut self,
        d: &mut Design<'a>,
        op: BinOp,
        va: Value,
        vb: Value,
        a: &'a Expr,
        b: &'a Expr,
        span: Span,
    ) -> R<Value> {
        let bop = match op {
            BinOp::And => BinaryOp::And,
            BinOp::Or => BinaryOp::Or,
            _ => BinaryOp::Xor,
        };
        // A constant takes the other operand's width.
        let (ha, hb) = match (va, vb) {
            (Value::Int(c, _), other) | (other, Value::Int(c, _)) => {
                let h = self.to_hw(d, other, span)?;
                let Ty::Int(i) = &h.ty else {
                    let tn = d.types.name(&h.ty);
                    return d.err(
                        Diagnostic::error(
                            span,
                            format!("`{}` between {tn} and an integer", op.symbol()),
                        )
                        .with_label("mismatched operands"),
                    );
                };
                let w = i.width();
                let t = IntTy::decl(IntDecl::Uint(w));
                if c.is_negative() || !t.contains_value(&c) {
                    return d.err(
                        Diagnostic::error(
                            span,
                            format!("{c} does not fit the {w} bits of the other operand"),
                        )
                        .with_label(format!("{w}-bit operation")),
                    );
                }
                let k = Hw {
                    sig: Sig::from_const(&c.to_logic_vec(w as usize)),
                    ty: Ty::Int(t),
                    tags: Tags::none(),
                };
                (h, k)
            }
            (x, y) => (self.to_hw(d, x, a.span)?, self.to_hw(d, y, b.span)?),
        };
        let tags = ha.tags.clone().joined(&hb.tags);
        match (&ha.ty, &hb.ty) {
            (Ty::Bool, Ty::Bool) => {
                let sig = d.out.binary(bop, false, ha.sig, hb.sig);
                Ok(Value::Hw(Hw { sig, ty: Ty::Bool, tags }))
            }
            (Ty::Int(ia), Ty::Int(ib)) => {
                for (i, e) in [(ia, a), (ib, b)] {
                    if i.signed() {
                        return d.err(
                            Diagnostic::error(
                                e.span,
                                format!(
                                    "`{}` works on unsigned bits, but this is signed",
                                    op.symbol()
                                ),
                            )
                            .with_label("signed")
                            .with_help("take the raw bits: `x.bits()`"),
                        );
                    }
                }
                let (wa, wb) = (ia.width(), ib.width());
                if wa != wb {
                    let (narrow, wide) = if wa < wb { (a, wb) } else { (b, wa) };
                    return d.err(
                        Diagnostic::error(
                            span,
                            format!(
                                "`{}` operands have different widths: {wa} and {wb} bits",
                                op.symbol()
                            ),
                        )
                        .with_label("mismatched widths")
                        .with_secondary(a.span, format!("{wa} bits"))
                        .with_secondary(b.span, format!("{wb} bits"))
                        .with_help(format!(
                            "widen the narrower one explicitly: `uint<{wide}>({})`",
                            d.snippet(narrow.span)
                        )),
                    );
                }
                let sig = d.out.binary(bop, false, ha.sig, hb.sig);
                Ok(Value::Hw(Hw { sig, ty: Ty::Int(IntTy::decl(IntDecl::Uint(wa))), tags }))
            }
            (ta, tb) => {
                let (an, bn) = (d.types.name(ta), d.types.name(tb));
                let mut dg = Diagnostic::error(
                    span,
                    format!(
                        "`{}` needs two `bool`s or two unsigned integers, not {an} and {bn}",
                        op.symbol()
                    ),
                )
                .with_label("mismatched operands");
                if matches!((ta, tb), (Ty::Bool, Ty::Int(_)) | (Ty::Int(_), Ty::Bool)) {
                    dg = dg.with_help("convert the `bool` with `.bits()`, or compare the integer");
                }
                d.err(dg)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn shift(
        &mut self,
        d: &mut Design<'a>,
        op: BinOp,
        va: Value,
        vb: Value,
        a: &'a Expr,
        b: &'a Expr,
        span: Span,
    ) -> R<Value> {
        let (ha, ia) = self.need_int(d, va, a, op)?;
        let w = ia.width();
        let rty = IntTy::decl(ia.natural_decl());
        let sig_a = ha.sig.clone();
        let (amt, tags) = match vb {
            Value::Int(n, _) => {
                if n.is_negative() {
                    return d.err(
                        Diagnostic::error(b.span, "shift amounts cannot be negative")
                            .with_label("negative"),
                    );
                }
                if n >= BigInt::from_u64(w as u64) {
                    let res = if op == BinOp::Shr && ia.signed() { "all sign bits" } else { "0" };
                    d.emit(
                        Diagnostic::warning(
                            span,
                            format!("shifting a {w}-bit value by {n} always gives {res}"),
                        )
                        .with_label("every bit is shifted out")
                        .with_help(
                            "shifts keep the width; to scale exactly, multiply or `concat` zeros",
                        ),
                    );
                }
                let n = n.to_u64().unwrap_or(u64::MAX).min(w as u64 + 1);
                (Sig::from_u64(n, bits_unsigned(&BigInt::from_u64(n)) as usize), ha.tags)
            }
            other => {
                let hb = self.to_hw(d, other, b.span)?;
                if !matches!(&hb.ty, Ty::Int(i) if !i.signed()) {
                    let tn = d.types.name(&hb.ty);
                    return d.err(
                        Diagnostic::error(
                            b.span,
                            format!("a shift amount must be unsigned, not {tn}"),
                        )
                        .with_label(tn),
                    );
                }
                let t = ha.tags.joined(&hb.tags);
                (hb.sig, t)
            }
        };
        let bop = match (op, ia.signed()) {
            (BinOp::Shl, _) => BinaryOp::Shl,
            (_, true) => BinaryOp::Sshr,
            _ => BinaryOp::Shr,
        };
        let sig = d.out.binary(bop, ia.signed(), sig_a, amt);
        Ok(Value::Hw(Hw { sig, ty: Ty::Int(rty), tags }))
    }

    pub fn eval_block(
        &mut self,
        d: &mut Design<'a>,
        b: &'a ast::ExprBlock,
        exp: Option<&Ty>,
    ) -> R<Value> {
        let rr = std::mem::replace(&mut self.reg_read, false);
        self.scopes.push(Scope::default());
        let r = (|| {
            for st in &b.lets {
                self.local_let(d, st)?;
            }
            self.reg_read = rr;
            self.eval(d, &b.value, exp)
        })();
        self.scopes.pop();
        r
    }

    fn eval_if(
        &mut self,
        d: &mut Design<'a>,
        c: &'a Expr,
        t: &'a ast::ExprBlock,
        els: &'a Expr,
        exp: Option<&Ty>,
        rr: bool,
    ) -> R<Value> {
        let cv = self.eval_hw(d, c, Some(&Ty::Bool))?;
        let cv = self.coerce_hw(d, cv, &Ty::Bool, c.span, "the condition", Some(c))?;
        let bit = cv.sig.0[0];
        if cv.tags.is_static() && bit.is_const() {
            self.reg_read = rr;
            return if bit == SigBit::ONE {
                self.eval_block(d, t, exp)
            } else {
                self.eval(d, els, exp)
            };
        }
        let (tn, en) = self.narrowing(d, c);
        self.scopes.push(Scope { narrow: tn, ..Default::default() });
        self.reg_read = rr;
        let tv = self.eval_block(d, t, exp);
        self.scopes.pop();
        self.scopes.push(Scope { narrow: en, ..Default::default() });
        self.reg_read = rr;
        let ev = self.eval(d, els, exp);
        self.scopes.pop();
        let (tv, ev) = (tv?, ev?);
        let (th, eh) = self.join_values(d, tv, ev, t.value.span, els.span, exp)?;
        let sig = d.out.mux(bit, eh.sig, th.sig);
        let tags = cv.tags.joined(&th.tags).joined(&eh.tags);
        Ok(Value::Hw(Hw { sig, ty: th.ty, tags }))
    }

    /// Brings two branch values to a common type.
    pub fn join_values(
        &mut self,
        d: &mut Design<'a>,
        a: Value,
        b: Value,
        sa: Span,
        sb: Span,
        exp: Option<&Ty>,
    ) -> R<(Hw, Hw)> {
        if let Some(t) = exp {
            let ha = self.coerce(d, a, t, sa, "this branch", None)?;
            let hb = self.coerce(d, b, t, sb, "this branch", None)?;
            return Ok((ha, hb));
        }
        let ha = self.to_hw(d, a, sa)?;
        let hb = self.to_hw(d, b, sb)?;
        let Some(t) = self.join_types(d, &ha.ty, &hb.ty) else {
            let (an, bn) = (d.types.name(&ha.ty), d.types.name(&hb.ty));
            return d.err(
                Diagnostic::error(sb, format!("branches have different types: {an} and {bn}"))
                    .with_label(bn)
                    .with_secondary(sa, an),
            );
        };
        let ha = self.coerce_hw(d, ha, &t, sa, "this branch", None)?;
        let hb = self.coerce_hw(d, hb, &t, sb, "this branch", None)?;
        Ok((ha, hb))
    }

    /// Range narrowing for the `then` and `else` branches of `c`.
    pub fn narrowing(
        &mut self,
        d: &mut Design<'a>,
        c: &'a Expr,
    ) -> (HashMap<String, IntTy>, HashMap<String, IntTy>) {
        let mut then = HashMap::new();
        let mut els = HashMap::new();
        let mut conj = Vec::new();
        fn flatten<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
            match &e.strip().kind {
                ExprKind::Binary(BinOp::LogicAnd, a, b) => {
                    flatten(a, out);
                    flatten(b, out);
                }
                _ => out.push(e.strip()),
            }
        }
        flatten(c, &mut conj);
        let single = conj.len() == 1;
        for e in conj {
            let ExprKind::Binary(op, a, b) = &e.kind else { continue };
            if !op.is_compare() || *op == BinOp::Ne {
                continue;
            }
            let (name, k, op) = match (&a.strip().kind, &b.strip().kind) {
                (ExprKind::Ident(n), _) => (n, b.as_ref(), *op),
                (_, ExprKind::Ident(n)) => {
                    let flipped = match op {
                        BinOp::Lt => BinOp::Gt,
                        BinOp::Le => BinOp::Ge,
                        BinOp::Gt => BinOp::Lt,
                        BinOp::Ge => BinOp::Le,
                        o => *o,
                    };
                    (n, a.as_ref(), flipped)
                }
                _ => continue,
            };
            let Some(cur) = self.current_int_range(d, &name.name) else { continue };
            let nd = d.diags.items.len();
            let kv = match self.eval(d, k, None) {
                Ok(Value::Int(v, _)) => v,
                Ok(Value::Hw(h)) if h.tags.is_static() && const_int(&h).is_some() => {
                    const_int(&h).unwrap()
                }
                _ => {
                    d.diags.items.truncate(nd);
                    continue;
                }
            };
            let base = then.get(&name.name).cloned().unwrap_or(cur.clone());
            let clip = |lo: BigInt, hi: BigInt, t: &IntTy| -> IntTy {
                let lo = if lo > t.lo { lo } else { t.lo.clone() };
                let hi = if hi < t.hi { hi } else { t.hi.clone() };
                IntTy { lo, hi, decl: t.decl.clone() }
            };
            let (tlo, thi, elo, ehi) = match op {
                BinOp::Lt => (cur.lo.clone(), &kv - &one(), kv.clone(), cur.hi.clone()),
                BinOp::Le => (cur.lo.clone(), kv.clone(), &kv + &one(), cur.hi.clone()),
                BinOp::Gt => (&kv + &one(), cur.hi.clone(), cur.lo.clone(), kv.clone()),
                BinOp::Ge => (kv.clone(), cur.hi.clone(), cur.lo.clone(), &kv - &one()),
                _ => (kv.clone(), kv.clone(), cur.lo.clone(), cur.hi.clone()),
            };
            let tn = clip(tlo, thi, &base);
            if tn.lo <= tn.hi {
                then.insert(name.name.clone(), tn);
            }
            if single && op != BinOp::Eq {
                let en = clip(elo, ehi, &cur);
                if en.lo <= en.hi {
                    els.insert(name.name.clone(), en);
                }
            }
        }
        (then, els)
    }

    /// The current (possibly narrowed) integer range of a name.
    pub fn current_int_range(&mut self, d: &mut Design<'a>, name: &str) -> Option<IntTy> {
        if let Some(n) = self.narrowed(name) {
            return Some(n.clone());
        }
        for s in self.scopes.iter().rev() {
            if let Some(sym) = s.syms.get(name) {
                return match sym {
                    Sym::Signal(i) => match &self.signals[*i].ty {
                        Ty::Int(t) => Some(t.clone()),
                        _ => None,
                    },
                    Sym::Var(h) | Sym::Value(Value::Hw(h)) => match &h.ty {
                        Ty::Int(t) => Some(t.clone()),
                        _ => None,
                    },
                    _ => None,
                };
            }
        }
        if self.lazy.contains_key(name) {
            let id = ast::Ident { name: name.to_string(), span: Span::default() };
            return match self.lookup(d, &id).ok()? {
                Some(Sym::Signal(i)) => match &self.signals[i].ty {
                    Ty::Int(t) => Some(t.clone()),
                    _ => None,
                },
                Some(Sym::Value(Value::Hw(h))) => match &h.ty {
                    Ty::Int(t) => Some(t.clone()),
                    _ => None,
                },
                _ => None,
            };
        }
        None
    }

    fn eval_struct(
        &mut self,
        d: &mut Design<'a>,
        name: &ast::Ident,
        fields: &'a [(ast::Ident, Expr)],
        span: Span,
    ) -> R<Value> {
        let Some(Ty::Struct(si)) = d.type_names.get(&name.name).cloned() else {
            let mut dg = Diagnostic::error(name.span, format!("`{}` is not a struct", name.name))
                .with_label("unknown struct");
            let cands: Vec<String> = d
                .type_names
                .iter()
                .filter(|(_, t)| matches!(t, Ty::Struct(_)))
                .map(|(k, _)| k.clone())
                .collect();
            if let Some(s) = suggest(&name.name, cands.iter().map(|s| s.as_str())) {
                dg = dg.with_help(format!("did you mean `{s}`?"));
            }
            return d.err(dg);
        };
        let info = d.types.structs[si].clone();
        let mut vals: HashMap<String, Hw> = HashMap::new();
        let mut tags = Tags::none();
        for (f, e) in fields {
            let Some((_, ft)) = info.fields.iter().find(|(n, _)| *n == f.name) else {
                let mut dg =
                    Diagnostic::error(f.span, format!("`{}` has no field `{}`", info.name, f.name))
                        .with_label("unknown field");
                if let Some(s) = suggest(&f.name, info.fields.iter().map(|(n, _)| n.as_str())) {
                    dg = dg.with_help(format!("did you mean `{s}`?"));
                }
                d.emit(dg);
                continue;
            };
            if vals.contains_key(&f.name) {
                d.emit(
                    Diagnostic::error(f.span, format!("field `{}` is given twice", f.name))
                        .with_label("duplicate"),
                );
                continue;
            }
            let h = self.eval_to(d, e, ft, &format!("field `{}`", f.name))?;
            tags.join(&h.tags);
            vals.insert(f.name.clone(), h);
        }
        let missing: Vec<String> = info
            .fields
            .iter()
            .filter(|(n, _)| !vals.contains_key(n))
            .map(|(n, _)| format!("`{n}`"))
            .collect();
        if !missing.is_empty() {
            return d.err(
                Diagnostic::error(
                    span,
                    format!(
                        "missing field{} {} of `{}`",
                        if missing.len() > 1 { "s" } else { "" },
                        missing.join(", "),
                        info.name
                    ),
                )
                .with_label("incomplete struct value")
                .with_help("every field must be given"),
            );
        }
        let mut sig = Sig::new();
        for (n, _) in info.fields.iter().rev() {
            sig = sig.concat(&vals[n].sig);
        }
        Ok(Value::Hw(Hw { sig, ty: Ty::Struct(si), tags }))
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_call(
        &mut self,
        d: &mut Design<'a>,
        callee: &'a ast::Ident,
        generics: &'a [ast::GenArg],
        args: &'a [Arg],
        dom: Option<&'a ast::DomRef>,
        span: Span,
        exp: Option<&Ty>,
        rr: bool,
    ) -> R<Value> {
        let name = callee.name.as_str();
        let positional = |d: &mut Design<'a>, n: usize| -> R<()> {
            if args.len() != n {
                return d.err(
                    Diagnostic::error(
                        span,
                        format!(
                            "`{name}` takes {n} argument{}, but {} were given",
                            if n == 1 { "" } else { "s" },
                            args.len()
                        ),
                    )
                    .with_label("wrong number of arguments"),
                );
            }
            if let Some(a) = args.iter().find_map(|a| a.name.as_ref()) {
                return d.err(
                    Diagnostic::error(a.span, format!("`{name}` takes positional arguments"))
                        .with_label("unexpected name"),
                );
            }
            Ok(())
        };
        if crate::BUILTINS.contains(&name) && !generics.is_empty() {
            return d.err(
                Diagnostic::error(span, format!("`{name}` takes no generic arguments"))
                    .with_label("unexpected `<...>`"),
            );
        }
        match name {
            "concat" => {
                if args.is_empty() {
                    return d.err(
                        Diagnostic::error(span, "`concat` needs at least one argument")
                            .with_label("empty"),
                    );
                }
                let mut sig = Sig::new();
                let mut tags = Tags::none();
                for a in args.iter().rev() {
                    let v = self.eval(d, &a.value, None)?;
                    let h = match v {
                        Value::Int(c, Some(w)) => Hw {
                            sig: Sig::from_const(&c.to_logic_vec(w as usize)),
                            ty: Ty::Int(IntTy::decl(IntDecl::Uint(w))),
                            tags: Tags::none(),
                        },
                        Value::Int(c, None) => {
                            return d.err(
                                Diagnostic::error(a.value.span, format!("{c} has no width in `concat`"))
                                    .with_label("decimal constant")
                                    .with_help(format!("give it a type (`uint<{}>({c})`), or write it in binary or hex, whose digits set the width", bits_unsigned(&c.abs())))
                            );
                        }
                        other => self.to_hw(d, other, a.value.span)?,
                    };
                    if !matches!(h.ty, Ty::Bool | Ty::Int(_)) {
                        let tn = d.types.name(&h.ty);
                        return d.err(
                            Diagnostic::error(
                                a.value.span,
                                format!("`concat` joins bools and integers, not {tn}"),
                            )
                            .with_label(tn)
                            .with_help("convert it with `.bits()` first"),
                        );
                    }
                    tags.join(&h.tags);
                    sig = sig.concat(&h.sig);
                }
                let w = sig.width() as u32;
                Ok(Value::Hw(Hw { sig, ty: Ty::Int(IntTy::decl(IntDecl::Uint(w))), tags }))
            }
            "from_bits" => {
                positional(d, 1)?;
                let Some(t) = exp.cloned() else {
                    return d.err(
                        Diagnostic::error(span, "the result type of `from_bits` is not known here")
                            .with_label("needs a type")
                            .with_help("use it where a type is expected: `let p: Pixel = from_bits(word);`"),
                    );
                };
                if !d.types.can_build_from_bits(&t) {
                    let tn = d.types.name(&t);
                    return d.err(
                        Diagnostic::error(span, format!("`from_bits` cannot build {tn}"))
                            .with_label("contains an enum or an index")
                            .with_help("not every bit pattern is a valid enum or index value: decode with `match` or a comparison"),
                    );
                }
                let w = d.types.width(&t);
                let v = self.eval(d, &args[0].value, None)?;
                let h = match v {
                    Value::Int(c, _) => {
                        let ut = IntTy::decl(IntDecl::Uint(w));
                        if !ut.contains_value(&c) {
                            return d.err(
                                Diagnostic::error(
                                    args[0].value.span,
                                    format!("{c} does not fit {w} bits"),
                                )
                                .with_label("too large"),
                            );
                        }
                        Hw {
                            sig: Sig::from_const(&c.to_logic_vec(w as usize)),
                            ty: Ty::Int(ut),
                            tags: Tags::none(),
                        }
                    }
                    other => self.to_hw(d, other, args[0].value.span)?,
                };
                if !matches!(&h.ty, Ty::Int(i) if !i.signed()) || h.sig.width() as u32 != w {
                    let tn = d.types.name(&t);
                    let hn = d.types.name(&h.ty);
                    return d.err(
                        Diagnostic::error(
                            args[0].value.span,
                            format!("{tn} is built from a uint<{w}>, but this is {hn}"),
                        )
                        .with_label("wrong width")
                        .with_help(format!("convert or slice it to exactly {w} bits")),
                    );
                }
                Ok(Value::Hw(Hw { sig: h.sig, ty: t, tags: h.tags }))
            }
            "unsafe_cross" => {
                positional(d, 1)?;
                self.reg_read = rr;
                let v = self.eval(d, &args[0].value, exp)?;
                let mut h = self.to_hw(d, v, args[0].value.span)?;
                h.tags = Tags::none();
                Ok(Value::Hw(h))
            }
            "clog2" | "bits_for" => {
                positional(d, 1)?;
                let n = self.eval_int(d, &args[0].value)?;
                if n.is_negative() || (name == "clog2" && n.is_zero()) {
                    return d.err(
                        Diagnostic::error(
                            args[0].value.span,
                            format!(
                                "`{name}` needs a {} argument",
                                if name == "clog2" { "positive" } else { "non-negative" }
                            ),
                        )
                        .with_label(format!("{n}")),
                    );
                }
                let v =
                    if name == "clog2" { (&n - &one()).bit_len() } else { (n.bit_len()).max(1) };
                Ok(Value::Int(BigInt::from_u64(v), None))
            }
            "min" | "max" => {
                positional(d, 2)?;
                let x = self.eval_int(d, &args[0].value)?;
                let y = self.eval_int(d, &args[1].value)?;
                let r = if (name == "min") == (x < y) { x } else { y };
                Ok(Value::Int(r, None))
            }
            "hex_file" | "bin_file" => d.err(
                Diagnostic::error(span, format!("`{name}` is only allowed as a memory's `init`"))
                    .with_label("not a value")
                    .with_help(format!("write `mem m: [uint<8>; 256] init {name}(\"file\");`")),
            ),
            "assert" => d.err(
                Diagnostic::error(span, "`assert` is a statement")
                    .with_label("not a value")
                    .with_help("write `assert(cond, \"message\");` on its own line"),
            ),
            _ => {
                if let Some(f) = d.fns.get(name).copied() {
                    if d.indexing {
                        d.note(callee.span, Some(f.name.span), RefKind::Function, d.fn_hover(f));
                    }
                    if dom.is_some() {
                        return d.err(
                            Diagnostic::error(span, "functions have no domain")
                                .with_label("unexpected `@`")
                                .with_help("a function's result is in the domain of its arguments"),
                        );
                    }
                    return self.call_fn(d, f, generics, args, span);
                }
                if let Some(m) = d.modules.get(name).copied() {
                    if !self.allow_module_call {
                        let help = if self.mode == Mode::Function {
                            "functions are pure logic and cannot instantiate modules".to_string()
                        } else {
                            format!(
                                "a module call creates hardware unconditionally: give it a name at module level, `let x = {name}(...);` or `inst x = {name}(...);`"
                            )
                        };
                        return d.err(
                            Diagnostic::error(
                                span,
                                format!("`{name}` is a module and cannot be called here"),
                            )
                            .with_label("module instantiation")
                            .with_help(help),
                        );
                    }
                    let base = self.call_name.clone().unwrap_or_else(|| format!("${name}"));
                    let iname = format!("{base}${}", self.uniq);
                    self.uniq += 1;
                    let inst =
                        self.instantiate(d, m, &iname, callee, generics, args, dom, span, true)?;
                    let outs: Vec<usize> = self.insts[inst]
                        .ports
                        .iter()
                        .map(|(_, s)| *s)
                        .filter(|&s| matches!(self.signals[s].kind, SigKind::InstOut { .. }))
                        .collect();
                    if outs.len() != 1 {
                        let names: Vec<String> = self.insts[inst]
                            .ports
                            .iter()
                            .filter(|(_, s)| {
                                matches!(self.signals[*s].kind, SigKind::InstOut { .. })
                            })
                            .map(|(n, _)| format!("`{n}`"))
                            .collect();
                        return d.err(
                            Diagnostic::error(span, format!("`{name}` has {} outputs, so it cannot be used as a value", outs.len()))
                                .with_label(if names.is_empty() { "no outputs".to_string() } else { format!("outputs {}", names.join(", ")) })
                                .with_help(format!("give the instance a name: `inst u = {name}(...);` and read `u.<port>`")),
                        );
                    }
                    let s = outs[0];
                    self.signals[s].read = true;
                    let sg = &self.signals[s];
                    let tags = match &sg.tag {
                        Some(FixedTag { atom: Some(a), what }) => {
                            Tags::one(*a, span, format!("`{name}(...)` ({what})"))
                        }
                        _ => Tags::none(),
                    };
                    return Ok(Value::Hw(Hw { sig: sg.sig.clone(), ty: sg.ty.clone(), tags }));
                }
                let mut cands: Vec<&str> = d.fns.keys().map(|s| s.as_str()).collect();
                cands.extend(d.modules.keys().map(|s| s.as_str()));
                cands.extend(crate::BUILTINS.iter().copied());
                let mut dg =
                    Diagnostic::error(callee.span, format!("unknown function or module `{name}`"))
                        .with_label("not defined");
                if let Some(s) = suggest(name, cands) {
                    dg = dg.with_help(format!("did you mean `{s}`?"));
                } else if name.starts_with('$')
                    || matches!(name, "signed" | "unsigned" | "readmemh")
                {
                    dg = dg.with_help("Verilog system functions are builtins or methods in Weft: `clog2(n)`, `x.as_sint()`, `init hex_file(\"...\")`");
                }
                d.err(dg)
            }
        }
    }

    /// Converts a constant value to the bits of `ty` (for initial values).
    pub fn const_bits(
        &mut self,
        d: &mut Design<'a>,
        e: &'a Expr,
        ty: &Ty,
        what: &str,
    ) -> R<LogicVec> {
        let h = self.eval_to(d, e, ty, what)?;
        match h.sig.as_const() {
            Some(c) if h.tags.is_static() && c.is_known() => Ok(c),
            Some(_) if h.tags.is_static() => d.err(
                Diagnostic::error(e.span, format!("{what} must be fully defined"))
                    .with_label("contains `undef`")
                    .with_help("power-on and reset values are always defined; pick a value"),
            ),
            _ => d.err(
                Diagnostic::error(e.span, format!("{what} must be a constant"))
                    .with_label("depends on signals")
                    .with_help("to load a value at runtime, assign it with `:=`"),
            ),
        }
    }
}

/// The common type of two branches.
fn join_types(a: &Ty, b: &Ty) -> Option<Ty> {
    match (a, b) {
        (Ty::Int(x), Ty::Int(y)) => Some(Ty::Int(hull(x, y))),
        (Ty::Array(x, n), Ty::Array(y, m)) if n == m => {
            Some(Ty::Array(Box::new(join_types(x, y)?), *n))
        }
        _ if a == b => Some(a.clone()),
        _ => None,
    }
}
