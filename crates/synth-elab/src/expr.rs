//! Expression typing (IEEE 1364-2005 §5.4–5.5 sizing and signedness),
//! generation of RTL for expressions, and assignment targets.

use synth_core::{Logic, LogicVec, Span};
use synth_rtl::{BinaryOp, MemRead, Sig, SigBit, UnaryOp};
use synth_verilog::ast::{self, BinaryOp as B, Expr, UnaryOp as U};

use crate::{Elab, MemInfo, Sym, VarInfo};

/// An assignment target.
#[derive(Clone, Debug)]
pub(crate) enum Lvalue {
    /// Fixed bits (wire bits), least significant first.
    Bits(Vec<SigBit>),
    /// A variable-indexed select `v[i]` / `v[i +: w]`: writes `width` bits
    /// at position `index + offset` of `base`.
    Dynamic { base: Vec<SigBit>, index: Sig, index_signed: bool, offset: i64, width: usize },
    /// A memory word (or part of one): `mem[addr][..]`.
    Mem { mem: MemInfo, addr: Sig, lo: u32, width: u32 },
}

impl Lvalue {
    pub fn width(&self) -> usize {
        match self {
            Lvalue::Bits(b) => b.len(),
            Lvalue::Dynamic { width, .. } => *width,
            Lvalue::Mem { width, .. } => *width as usize,
        }
    }
}

/// A vector with its declared index range, as the base of a select.
struct SelBase {
    bits: Sig,
    msb: i64,
    lsb: i64,
    signed: bool,
}

impl SelBase {
    fn pos(&self, i: i64) -> Option<usize> {
        let p = if self.msb >= self.lsb { i - self.lsb } else { self.lsb - i };
        (p >= 0 && (p as usize) < self.bits.width()).then_some(p as usize)
    }
}

/// Converts a literal to a value and signedness.
pub(crate) fn number_value(n: &ast::Number) -> Option<(LogicVec, bool)> {
    match n.base {
        None => {
            let v = LogicVec::parse_radix(&n.digits, 10, 64)?;
            // Unsized decimal: at least 32 bits, signed.
            let needed = (0..64).rev().find(|&i| v.get(i) == Logic::One).map_or(1, |i| i + 2);
            Some((v.resize(needed.max(32), false), true))
        }
        Some('u') => {
            let b = Logic::from_char(n.digits.chars().next()?)?;
            Some((LogicVec::filled(1, b), false))
        }
        Some(base) => {
            let radix = match base {
                'b' => 2,
                'o' => 8,
                'd' => 10,
                'h' => 16,
                _ => return None,
            };
            let width = match n.size {
                Some(s) => s as usize,
                None => {
                    let bits_per = match radix {
                        2 => 1,
                        8 => 3,
                        16 => 4,
                        _ => 4,
                    };
                    (n.digits.len() * bits_per).max(32)
                }
            };
            Some((LogicVec::parse_radix(&n.digits, radix, width)?, n.signed))
        }
    }
}

fn is_unbased_unsized(e: &Expr) -> Option<Logic> {
    match e {
        Expr::Number(n, _) if n.base == Some('u') => Logic::from_char(n.digits.chars().next()?),
        _ => None,
    }
}

impl<'a> Elab<'a> {
    /// Reads variable bits, taking procedural assignments into account.
    pub(crate) fn read_bits(&self, bits: &Sig) -> Sig {
        match &self.proc {
            Some(p) => bits.iter().map(|b| p.current(*b)).collect(),
            None => bits.clone(),
        }
    }

    fn var_sig(&self, v: &VarInfo) -> Sig {
        self.read_bits(&Sig::wire(v.wire, v.width))
    }

    /// Self-determined width and signedness of an expression.
    pub(crate) fn expr_type(&mut self, e: &Expr) -> (u32, bool) {
        match e {
            Expr::Number(n, _) => match number_value(n) {
                Some((v, s)) => (v.width() as u32, s),
                None => (32, false),
            },
            Expr::Real(..) => (64, true),
            Expr::Str(s, _) => ((s.len() * 8).max(8) as u32, false),
            Expr::Ident(parts, _) => match self.lookup_path(parts) {
                Some(Sym::Var(v)) => (v.width, v.signed),
                Some(Sym::Param { value, signed }) => (value.width() as u32, signed),
                _ => (1, false),
            },
            Expr::Index { base, .. } => {
                if let Some(m) = self.mem_of(base) {
                    (m.width, m.signed)
                } else {
                    (1, false)
                }
            }
            Expr::Range { base, msb, lsb, .. } => {
                let _ = base;
                let (Some(a), Some(b)) = (self.try_const_int(msb), self.try_const_int(lsb)) else {
                    return (1, false);
                };
                ((a - b).unsigned_abs() as u32 + 1, false)
            }
            Expr::IndexedRange { width, .. } => {
                (self.try_const_int(width).unwrap_or(1).max(1) as u32, false)
            }
            Expr::Unary { op, expr, .. } => match op {
                U::Plus | U::Minus | U::BitNot => self.expr_type(expr),
                _ => (1, false),
            },
            Expr::Binary { op, lhs, rhs, .. } => match op {
                B::Add
                | B::Sub
                | B::Mul
                | B::Div
                | B::Mod
                | B::BitAnd
                | B::BitOr
                | B::BitXor
                | B::BitXnor => {
                    let (wa, sa) = self.expr_type(lhs);
                    let (wb, sb) = self.expr_type(rhs);
                    (wa.max(wb), sa && sb)
                }
                B::Pow => {
                    let (wa, sa) = self.expr_type(lhs);
                    let (_, sb) = self.expr_type(rhs);
                    (wa, sa && sb)
                }
                B::Shl | B::Shr | B::AShl | B::AShr => self.expr_type(lhs),
                _ => (1, false),
            },
            Expr::Ternary { then, otherwise, .. } => {
                let (wa, sa) = self.expr_type(then);
                let (wb, sb) = self.expr_type(otherwise);
                (wa.max(wb), sa && sb)
            }
            Expr::Concat(parts, _) => {
                (parts.iter().map(|p| self.expr_type(p).0).sum::<u32>().max(1), false)
            }
            Expr::Repeat { count, exprs, .. } => {
                let n = self.try_const_int(count).unwrap_or(1).max(0) as u32;
                (n * exprs.iter().map(|p| self.expr_type(p).0).sum::<u32>(), false)
            }
            Expr::Call { name, .. } => match self.lookup(&name.name).cloned() {
                Some(Sym::Func(f)) => {
                    if f.ret_kind == Some(ast::NetKind::Integer) {
                        (32, true)
                    } else {
                        let r = self.range(&f.range).unwrap_or((0, 0));
                        ((r.0 - r.1).unsigned_abs() as u32 + 1, f.signed)
                    }
                }
                _ => (1, false),
            },
            Expr::SysCall { name, args, .. } => match name.name.as_str() {
                "signed" | "unsigned" => {
                    let w = args.first().map_or(1, |a| self.expr_type(a).0);
                    (w, name.name == "signed")
                }
                "clog2" | "bits" | "size" => (32, true),
                _ => (32, false),
            },
        }
    }

    /// Like [`Elab::const_int`] but without reporting errors.
    pub(crate) fn try_const_int(&mut self, e: &Expr) -> Option<i64> {
        let n = self.diags.items.len();
        let saved = self.proc.take();
        let (w, s) = self.expr_type(e);
        let sig = self.emit(e, w, s);
        self.proc = saved;
        self.diags.items.truncate(n);
        sig.as_const()?.to_i64(s)
    }

    fn mem_of(&mut self, e: &Expr) -> Option<MemInfo> {
        match e {
            Expr::Ident(parts, _) => match self.lookup_path(parts) {
                Some(Sym::Mem(m)) => Some(m),
                _ => None,
            },
            _ => None,
        }
    }

    /// Generates `e` in a context of `width` bits and signedness `signed`.
    /// The result has exactly `width` bits.
    pub(crate) fn emit(&mut self, e: &Expr, width: u32, signed: bool) -> Sig {
        let w = width as usize;
        let r = self.gen_inner(e, width, signed);
        if r.width() == w { r } else { r.extend(w, false) }
    }

    fn gen_self(&mut self, e: &Expr) -> (Sig, bool) {
        let (w, s) = self.expr_type(e);
        (self.emit(e, w, s), s)
    }

    fn gen_inner(&mut self, e: &Expr, width: u32, signed: bool) -> Sig {
        let w = width as usize;
        match e {
            Expr::Number(n, span) => {
                if let Some(b) = is_unbased_unsized(e) {
                    return Sig(vec![SigBit::Const(b); w]);
                }
                match number_value(n) {
                    Some((v, s)) => Sig::from_const(&v.resize(w, s && signed)),
                    None => {
                        self.error(*span, "invalid number");
                        Sig::xs(w)
                    }
                }
            }
            Expr::Real(_, span) => {
                self.error(*span, "real numbers are not synthesizable");
                Sig::xs(w)
            }
            Expr::Str(s, _) => {
                let mut bits = Vec::new();
                for byte in s.bytes().rev() {
                    for i in 0..8 {
                        bits.push(SigBit::Const(Logic::from_bool(byte >> i & 1 != 0)));
                    }
                }
                if bits.is_empty() {
                    bits = vec![SigBit::ZERO; 8];
                }
                Sig(bits).extend(w, false)
            }
            Expr::Ident(parts, span) => match self.lookup_path(parts) {
                Some(Sym::Var(v)) => self.var_sig(&v).extend(w, signed && v.signed),
                Some(Sym::Param { value, signed: ps }) => {
                    Sig::from_const(&value.resize(w, ps && signed))
                }
                Some(Sym::Mem(_)) => {
                    self.error(*span, "a memory cannot be used as a value; index it");
                    Sig::xs(w)
                }
                Some(Sym::Genvar) => {
                    self.error(*span, "genvar used outside of a generate loop");
                    Sig::xs(w)
                }
                Some(_) => {
                    self.error(*span, "not a value");
                    Sig::xs(w)
                }
                None => {
                    let name =
                        parts.iter().map(|p| p.0.name.as_str()).collect::<Vec<_>>().join(".");
                    self.error(*span, format!("undeclared identifier `{name}`"));
                    Sig::xs(w)
                }
            },
            Expr::Index { base, index, span } => {
                if let Some(m) = self.mem_of(base) {
                    let word = self.mem_read(&m, index);
                    return word.extend(w, signed && m.signed);
                }
                let Some(b) = self.sel_base(base) else { return Sig::xs(w) };
                let bit = self.bit_select(&b, index, *span);
                Sig::bit(bit).extend(w, false)
            }
            Expr::Range { base, msb, lsb, span } => {
                let Some(b) = self.sel_base(base) else { return Sig::xs(w) };
                let (Some(m), Some(l)) = (self.const_int(msb), self.const_int(lsb)) else {
                    return Sig::xs(w);
                };
                let r = self.part_select(&b, m, l, *span);
                r.extend(w, false)
            }
            Expr::IndexedRange { base, start, width: wexpr, up, span } => {
                let Some(b) = self.sel_base(base) else { return Sig::xs(w) };
                let Some(pw) = self.const_int(wexpr) else { return Sig::xs(w) };
                if pw <= 0 {
                    self.error(*span, "part-select width must be positive");
                    return Sig::xs(w);
                }
                let r = self.indexed_select(&b, start, pw as usize, *up, *span);
                r.extend(w, false)
            }
            Expr::Unary { op, expr, .. } => match op {
                U::Plus => self.emit(expr, width, signed),
                U::Minus => {
                    let a = self.emit(expr, width, signed);
                    self.out.unary(UnaryOp::Neg, a)
                }
                U::BitNot => {
                    let a = self.emit(expr, width, signed);
                    self.out.unary(UnaryOp::Not, a)
                }
                _ => {
                    let (a, _) = self.gen_self(expr);
                    let (uop, invert) = match op {
                        U::LogicNot => (UnaryOp::LogicNot, false),
                        U::RedAnd => (UnaryOp::ReduceAnd, false),
                        U::RedNand => (UnaryOp::ReduceAnd, true),
                        U::RedOr => (UnaryOp::ReduceOr, false),
                        U::RedNor => (UnaryOp::ReduceOr, true),
                        U::RedXor => (UnaryOp::ReduceXor, false),
                        U::RedXnor => (UnaryOp::ReduceXnor, false),
                        _ => unreachable!(),
                    };
                    let mut r = self.out.unary(uop, a);
                    if invert {
                        r = self.out.unary(UnaryOp::Not, r);
                    }
                    r.extend(w, false)
                }
            },
            Expr::Binary { op, lhs, rhs, span } => {
                self.gen_binary(*op, lhs, rhs, width, signed, *span)
            }
            Expr::Ternary { cond, then, otherwise, .. } => {
                let (c, _) = self.gen_self(cond);
                let c = self.out.reduce_bool(c);
                let t = self.emit(then, width, signed);
                let f = self.emit(otherwise, width, signed);
                self.out.mux(c, f, t)
            }
            Expr::Concat(parts, _) => {
                let mut acc = Sig::new();
                for p in parts.iter().rev() {
                    if let Expr::Repeat { count, .. } = p
                        && self.try_const_int(count) == Some(0)
                    {
                        continue;
                    }
                    let (s, _) = self.gen_self(p);
                    acc = acc.concat(&s);
                }
                acc.extend(w.max(acc.width()), false)
            }
            Expr::Repeat { count, exprs, span } => {
                let Some(n) = self.const_int(count) else { return Sig::xs(w) };
                if n < 0 {
                    self.error(*span, "negative replication count");
                    return Sig::xs(w);
                }
                let mut unit = Sig::new();
                for p in exprs.iter().rev() {
                    let (s, _) = self.gen_self(p);
                    unit = unit.concat(&s);
                }
                let mut acc = Sig::new();
                for _ in 0..n {
                    acc = acc.concat(&unit);
                }
                acc.extend(w.max(acc.width()), false)
            }
            Expr::Call { name, args, span } => match self.lookup(&name.name).cloned() {
                Some(Sym::Func(f)) => {
                    let r = self.call_function(f, args, *span);
                    let (_, fs) = self.expr_type(e);
                    r.extend(w, fs && signed)
                }
                _ => {
                    self.error(name.span, format!("`{}` is not a function", name.name));
                    Sig::xs(w)
                }
            },
            Expr::SysCall { name, args, span } => {
                self.sys_call(&name.name, args, width, signed, *span)
            }
        }
    }

    fn sys_call(&mut self, name: &str, args: &[Expr], width: u32, signed: bool, span: Span) -> Sig {
        let w = width as usize;
        match name {
            "signed" | "unsigned" => {
                let Some(a) = args.first() else {
                    self.error(span, format!("${name} needs an argument"));
                    return Sig::xs(w);
                };
                let (aw, _) = self.expr_type(a);
                let inner = self.emit(a, aw, name == "signed");
                inner.extend(w, signed && name == "signed")
            }
            "clog2" => {
                let Some(a) = args.first() else { return Sig::xs(w) };
                match self.const_expr(a) {
                    Some((v, _)) => Sig::from_const(
                        &LogicVec::from_u64(v.clog2().unwrap_or(0), 32).resize(w, false),
                    ),
                    None => Sig::xs(w),
                }
            }
            "bits" | "size" => {
                let Some(a) = args.first() else { return Sig::xs(w) };
                let (aw, _) = self.expr_type(a);
                Sig::from_u64(aw as u64, w)
            }
            "time" | "stime" | "realtime" | "random" | "urandom" => {
                self.warn_once(span, format!("${name} is not synthesizable; using x"));
                Sig::xs(w)
            }
            _ => {
                self.error(span, format!("unsupported system function ${name}"));
                Sig::xs(w)
            }
        }
    }

    fn gen_binary(
        &mut self,
        op: B,
        lhs: &Expr,
        rhs: &Expr,
        width: u32,
        signed: bool,
        span: Span,
    ) -> Sig {
        let w = width as usize;
        let arith = |op: B| match op {
            B::Add => Some(BinaryOp::Add),
            B::Sub => Some(BinaryOp::Sub),
            B::Mul => Some(BinaryOp::Mul),
            B::Div => Some(BinaryOp::Div),
            B::Mod => Some(BinaryOp::Mod),
            B::BitAnd => Some(BinaryOp::And),
            B::BitOr => Some(BinaryOp::Or),
            B::BitXor => Some(BinaryOp::Xor),
            B::BitXnor => Some(BinaryOp::Xnor),
            _ => None,
        };
        if let Some(rop) = arith(op) {
            let a = self.emit(lhs, width, signed);
            let b = self.emit(rhs, width, signed);
            return self.out.binary(rop, signed, a, b);
        }
        match op {
            B::Shl | B::Shr | B::AShl | B::AShr => {
                let a = self.emit(lhs, width, signed);
                let (b, _) = self.gen_self(rhs);
                let rop = match op {
                    B::Shl | B::AShl => BinaryOp::Shl,
                    B::AShr if signed => BinaryOp::Sshr,
                    _ => BinaryOp::Shr,
                };
                self.out.binary(rop, signed, a, b)
            }
            B::Pow => {
                let a = self.emit(lhs, width, signed);
                let (b, bs) = self.gen_self(rhs);
                match (a.as_const(), b.as_const()) {
                    (Some(ca), Some(cb)) => Sig::from_const(&ca.pow(&cb, signed, bs)),
                    (Some(ca), None) if ca.to_u64() == Some(2) => {
                        // 2 ** n = 1 << n
                        let one = Sig::from_u64(1, w);
                        self.out.binary(BinaryOp::Shl, false, one, b)
                    }
                    _ => {
                        self.error(
                            span,
                            "non-constant exponentiation is only supported with base 2",
                        );
                        Sig::xs(w)
                    }
                }
            }
            B::Eq | B::Ne | B::CaseEq | B::CaseNe | B::Lt | B::Le | B::Gt | B::Ge => {
                let (wa, sa) = self.expr_type(lhs);
                let (wb, sb) = self.expr_type(rhs);
                let m = wa.max(wb);
                let ss = sa && sb;
                let a = self.emit(lhs, m, ss);
                let b = self.emit(rhs, m, ss);
                let rop = match op {
                    B::Eq => BinaryOp::Eq,
                    B::Ne => BinaryOp::Ne,
                    B::CaseEq => BinaryOp::Eqx,
                    B::CaseNe => BinaryOp::Nex,
                    B::Lt => BinaryOp::Lt,
                    B::Le => BinaryOp::Le,
                    B::Gt => BinaryOp::Gt,
                    _ => BinaryOp::Ge,
                };
                // In hardware, === behaves like == (x does not exist).
                let rop = if !(a.is_const() && b.is_const()) {
                    match rop {
                        BinaryOp::Eqx => BinaryOp::Eq,
                        BinaryOp::Nex => BinaryOp::Ne,
                        o => o,
                    }
                } else {
                    rop
                };
                self.out.binary(rop, ss, a, b).extend(w, false)
            }
            B::LogicAnd | B::LogicOr => {
                let (a, _) = self.gen_self(lhs);
                let a = self.out.reduce_bool(a);
                let (b, _) = self.gen_self(rhs);
                let b = self.out.reduce_bool(b);
                let r = if op == B::LogicAnd { self.out.and1(a, b) } else { self.out.or1(a, b) };
                Sig::bit(r).extend(w, false)
            }
            _ => unreachable!(),
        }
    }

    /// The base of a select: a variable or parameter with its declared range,
    /// or a memory word (`mem[i]`).
    fn sel_base(&mut self, base: &Expr) -> Option<SelBase> {
        match base {
            Expr::Ident(parts, span) => match self.lookup_path(parts) {
                Some(Sym::Var(v)) => Some(SelBase {
                    bits: self.var_sig(&v),
                    msb: v.msb,
                    lsb: v.lsb,
                    signed: v.signed,
                }),
                Some(Sym::Param { value, signed }) => Some(SelBase {
                    bits: Sig::from_const(&value),
                    msb: value.width() as i64 - 1,
                    lsb: 0,
                    signed,
                }),
                Some(Sym::Mem(_)) => {
                    self.error(*span, "a memory must be indexed before selecting bits");
                    None
                }
                _ => {
                    self.error(*span, "undeclared identifier");
                    None
                }
            },
            Expr::Index { base: inner, index, .. } => {
                if let Some(m) = self.mem_of(inner) {
                    let word = self.mem_read(&m, index);
                    return Some(SelBase { bits: word, msb: m.msb, lsb: m.lsb, signed: m.signed });
                }
                // Select of a select: treat the inner result as a plain vector.
                let (s, sg) = self.gen_self(base);
                Some(SelBase { msb: s.width() as i64 - 1, lsb: 0, bits: s, signed: sg })
            }
            other => {
                let (s, sg) = self.gen_self(other);
                Some(SelBase { msb: s.width() as i64 - 1, lsb: 0, bits: s, signed: sg })
            }
        }
    }

    fn bit_select(&mut self, b: &SelBase, index: &Expr, _span: Span) -> SigBit {
        let (idx, idx_signed) = self.gen_self(index);
        if let Some(c) = idx.as_const() {
            return match c.to_i64(idx_signed).and_then(|i| b.pos(i)) {
                Some(p) => b.bits.0[p],
                None => SigBit::X,
            };
        }
        let r = self.dynamic_select(b, idx, idx_signed, 1);
        r.0[0]
    }

    /// `bits[pos(index) +: width]` for a variable index.
    fn dynamic_select(&mut self, b: &SelBase, idx: Sig, idx_signed: bool, width: usize) -> Sig {
        if b.msb >= b.lsb {
            self.out.shiftx(b.bits.clone(), idx, idx_signed, -b.lsb, width)
        } else {
            // Descending declaration: position = lsb - index (width 1 only
            // maps cleanly; wider selects use the lowest position).
            let iw = idx.width().max(32) + 1;
            let lsb = Sig::from_const(&LogicVec::from_i64(b.lsb - (width as i64 - 1), iw));
            let i = idx.extend(iw, idx_signed);
            let pos = self.out.binary(BinaryOp::Sub, true, lsb, i);
            self.out.shiftx(b.bits.clone(), pos, true, 0, width)
        }
    }

    fn part_select(&mut self, b: &SelBase, m: i64, l: i64, span: Span) -> Sig {
        let _ = b.signed;
        // Positions of the selected indices, from l (least significant) to m.
        let width = (m - l).unsigned_abs() as usize + 1;
        if (b.msb >= b.lsb) != (m >= l) && m != l {
            self.error(span, "part-select direction does not match the declaration");
        }
        let step: i64 = if m >= l { 1 } else { -1 };
        (0..width as i64)
            .map(|k| {
                let i = l + k * step;
                b.pos(i).map_or(SigBit::X, |p| b.bits.0[p])
            })
            .collect()
    }

    fn indexed_select(
        &mut self,
        b: &SelBase,
        start: &Expr,
        width: usize,
        up: bool,
        span: Span,
    ) -> Sig {
        let (s, ss) = self.gen_self(start);
        if let Some(c) = s.as_const() {
            let Some(st) = c.to_i64(ss) else { return Sig::xs(width) };
            let (lo, hi) =
                if up { (st, st + width as i64 - 1) } else { (st - width as i64 + 1, st) };
            let (m, l) = if b.msb >= b.lsb { (hi, lo) } else { (lo, hi) };
            return self.part_select(b, m, l, span);
        }
        if b.msb < b.lsb {
            self.error(
                span,
                "variable indexed part-select of an ascending vector is not supported",
            );
            return Sig::xs(width);
        }
        let idx = if up {
            s
        } else {
            let iw = s.width().max(32) + 1;
            let ext = s.extend(iw, ss);
            let k = Sig::from_u64(width as u64 - 1, iw);
            self.out.binary(BinaryOp::Sub, ss, ext, k)
        };
        self.dynamic_select(b, idx, ss, width)
    }

    /// An asynchronous read of `m` at `index`.
    pub(crate) fn mem_read(&mut self, m: &MemInfo, index: &Expr) -> Sig {
        let (addr, _) = self.gen_self(index);
        let mem = &self.out.memories[m.mem];
        // Constant address with known initial contents and no writes yet
        // cannot be folded (writes may come later); always make a port.
        let _ = mem;
        let data = self.out.new_sig(m.width as usize);
        self.out.memories[m.mem].reads.push(MemRead {
            clk: None,
            en: SigBit::ONE,
            addr,
            data: data.clone(),
            transparent: vec![],
        });
        data
    }

    /// Resolves an assignment target. With `implicit`, an undeclared simple
    /// identifier declares a 1-bit wire (Verilog implicit nets).
    pub(crate) fn lvalue(&mut self, e: &Expr, implicit: bool) -> Option<Lvalue> {
        match e {
            Expr::Ident(parts, span) => match self.lookup_path(parts) {
                Some(Sym::Var(v)) => Some(Lvalue::Bits(Sig::wire(v.wire, v.width).0)),
                Some(_) => {
                    self.error(*span, "assignment target is not a variable or net");
                    None
                }
                None if implicit && parts.len() == 1 => {
                    let name = format!("{}{}", self.prefix(), parts[0].0.name);
                    let w = self.new_wire(&name, 1, false);
                    self.declare(
                        &parts[0].0.name,
                        Sym::Var(VarInfo {
                            wire: w,
                            width: 1,
                            signed: false,
                            msb: 0,
                            lsb: 0,
                            is_var: false,
                        }),
                        *span,
                    );
                    self.warn_once(*span, format!("implicit net `{}` declared", parts[0].0.name));
                    Some(Lvalue::Bits(vec![SigBit::Wire(w, 0)]))
                }
                None => {
                    self.error(*span, "undeclared assignment target");
                    None
                }
            },
            Expr::Index { base, index, span } => {
                if let Some(m) = self.mem_of(base) {
                    let (addr, _) = self.gen_self(index);
                    return Some(Lvalue::Mem { width: m.width, mem: m, addr, lo: 0 });
                }
                let v = self.lvalue_var(base)?;
                let (idx, is) = self.gen_self(index);
                if let Some(c) = idx.as_const() {
                    return match c.to_i64(is).and_then(|i| v.bit_of(i)) {
                        Some(p) => Some(Lvalue::Bits(vec![SigBit::Wire(v.wire, p)])),
                        None => {
                            self.warn_once(*span, "assignment to an out-of-range bit is ignored");
                            Some(Lvalue::Bits(vec![]))
                        }
                    };
                }
                if v.msb < v.lsb {
                    self.error(*span, "variable index into an ascending vector is not supported");
                    return None;
                }
                Some(Lvalue::Dynamic {
                    base: Sig::wire(v.wire, v.width).0,
                    index: idx,
                    index_signed: is,
                    offset: -v.lsb,
                    width: 1,
                })
            }
            Expr::Range { base, msb, lsb, span } => {
                if let Expr::Index { base: mb, index, .. } = base.as_ref()
                    && let Some(m) = self.mem_of(mb)
                {
                    let (addr, _) = self.gen_self(index);
                    let (hi, lo) = (self.const_int(msb)?, self.const_int(lsb)?);
                    let (Some(ph), Some(pl)) = (m.word_bit(hi), m.word_bit(lo)) else {
                        self.error(*span, "part-select out of range");
                        return None;
                    };
                    return Some(Lvalue::Mem {
                        width: ph.abs_diff(pl) + 1,
                        lo: ph.min(pl),
                        mem: m,
                        addr,
                    });
                }
                let v = self.lvalue_var(base)?;
                let (hi, lo) = (self.const_int(msb)?, self.const_int(lsb)?);
                let step: i64 = if hi >= lo { 1 } else { -1 };
                let n = (hi - lo).unsigned_abs() + 1;
                let mut bits = Vec::new();
                for k in 0..n as i64 {
                    match v.bit_of(lo + k * step) {
                        Some(p) => bits.push(SigBit::Wire(v.wire, p)),
                        None => {
                            self.error(*span, "part-select out of range");
                            return None;
                        }
                    }
                }
                Some(Lvalue::Bits(bits))
            }
            Expr::IndexedRange { base, start, width, up, span } => {
                if let Expr::Index { base: mb, index, .. } = base.as_ref()
                    && let Some(m) = self.mem_of(mb)
                {
                    let (addr, _) = self.gen_self(index);
                    let st = self.const_int(start)?;
                    let pw = self.const_int(width)?;
                    let lo_idx = if *up { st } else { st - pw + 1 };
                    let Some(pl) =
                        m.word_bit(if m.msb >= m.lsb { lo_idx } else { lo_idx + pw - 1 })
                    else {
                        self.error(*span, "part-select out of range");
                        return None;
                    };
                    return Some(Lvalue::Mem { width: pw as u32, lo: pl, mem: m, addr });
                }
                let v = self.lvalue_var(base)?;
                let pw = self.const_int(width)? as usize;
                let (s, ss) = self.gen_self(start);
                if let Some(c) = s.as_const() {
                    let st = c.to_i64(ss)?;
                    let lo = if *up { st } else { st - pw as i64 + 1 };
                    let mut bits = Vec::new();
                    for k in 0..pw as i64 {
                        let i = if v.msb >= v.lsb { lo + k } else { lo + pw as i64 - 1 - k };
                        match v.bit_of(i) {
                            Some(p) => bits.push(SigBit::Wire(v.wire, p)),
                            None => {
                                self.error(*span, "part-select out of range");
                                return None;
                            }
                        }
                    }
                    return Some(Lvalue::Bits(bits));
                }
                if v.msb < v.lsb {
                    self.error(
                        *span,
                        "variable part-select of an ascending vector is not supported",
                    );
                    return None;
                }
                let index = if *up {
                    s
                } else {
                    let iw = s.width().max(32) + 1;
                    let e = s.extend(iw, ss);
                    self.out.binary(BinaryOp::Sub, ss, e, Sig::from_u64(pw as u64 - 1, iw))
                };
                Some(Lvalue::Dynamic {
                    base: Sig::wire(v.wire, v.width).0,
                    index,
                    index_signed: ss,
                    offset: -v.lsb,
                    width: pw,
                })
            }
            Expr::Concat(parts, _) => {
                let mut bits = Vec::new();
                for p in parts.iter().rev() {
                    match self.lvalue(p, implicit)? {
                        Lvalue::Bits(b) => bits.extend(b),
                        _ => {
                            self.error(
                                p.span(),
                                "concatenated assignment targets must be fixed selects",
                            );
                            return None;
                        }
                    }
                }
                Some(Lvalue::Bits(bits))
            }
            other => {
                self.error(other.span(), "invalid assignment target");
                None
            }
        }
    }

    fn lvalue_var(&mut self, base: &Expr) -> Option<VarInfo> {
        match base {
            Expr::Ident(parts, span) => match self.lookup_path(parts) {
                Some(Sym::Var(v)) => Some(v),
                _ => {
                    self.error(*span, "assignment target is not a variable or net");
                    None
                }
            },
            other => {
                self.error(other.span(), "unsupported assignment target");
                None
            }
        }
    }
}
