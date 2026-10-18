//! `match` patterns: conditions, exhaustiveness and reachability.
//!
//! Integer patterns are sets of values in the scrutinee's bit space (its
//! natural representation): intervals for literals and ranges, cubes for
//! bit patterns. Coverage is decided exactly by splitting the bit space on
//! its leading bits until every region is either fully covered by a pattern
//! or misses all of them.

use synth_core::{BigInt, Diagnostic, Span};
use synth_rtl::{BinaryOp, Sig, SigBit};

use super::*;
use crate::ast::{Expr, PatKind, Pattern};

#[derive(Clone, Debug)]
enum Atom {
    Interval(u128, u128),
    Cube { value: u128, care: u128 },
}

#[derive(Copy, Clone, PartialEq)]
enum Rel {
    Disjoint,
    Contains,
    Partial,
}

fn rel(a: &Atom, p: u128, k: u32) -> Rel {
    let lo = p << k;
    let free = if k == 0 { 0 } else { (1u128 << k) - 1 };
    let hi = lo | free;
    match a {
        Atom::Interval(x, y) => {
            if *y < lo || *x > hi {
                Rel::Disjoint
            } else if *x <= lo && hi <= *y {
                Rel::Contains
            } else {
                Rel::Partial
            }
        }
        Atom::Cube { value, care } => {
            if (lo ^ value) & care & !free != 0 {
                Rel::Disjoint
            } else if care & free == 0 {
                Rel::Contains
            } else {
                Rel::Partial
            }
        }
    }
}

/// A value of `target` not covered by `pats`, if any (`w` bits).
fn uncovered(target: &Atom, pats: &[&Atom], w: u32) -> Option<u128> {
    fn go(target: &Atom, pats: &[&Atom], p: u128, k: u32) -> Option<u128> {
        if rel(target, p, k) == Rel::Disjoint {
            return None;
        }
        let live: Vec<&Atom> =
            pats.iter().copied().filter(|a| rel(a, p, k) != Rel::Disjoint).collect();
        if live.iter().any(|a| rel(a, p, k) == Rel::Contains) {
            return None;
        }
        if live.is_empty() {
            // Any value of the target in this region.
            let lo = p << k;
            let free = if k == 0 { 0 } else { (1u128 << k) - 1 };
            return Some(match target {
                Atom::Interval(x, _) => (*x).max(lo),
                Atom::Cube { value, care } => lo | (value & care & free),
            });
        }
        if k == 0 {
            return None;
        }
        go(target, &live, p << 1, k - 1).or_else(|| go(target, &live, (p << 1) | 1, k - 1))
    }
    go(target, pats, 0, w)
}

/// One arm's match condition, value set and narrowed scrutinee range.
pub(crate) struct ArmInfo {
    pub cond: SigBit,
    pub narrow: Option<IntTy>,
}

enum Set {
    Int(Vec<Atom>),
    Variants(Vec<usize>),
    All,
}

impl<'a> ModCx<'a> {
    /// Conditions of each arm (before priority), after checking that the
    /// arms are exhaustive and all reachable.
    pub fn match_arms(
        &mut self,
        d: &mut Design<'a>,
        scr: &Hw,
        scr_e: &'a Expr,
        pats: &[&'a Pattern],
    ) -> R<Vec<ArmInfo>> {
        let mut infos = Vec::new();
        let mut sets = Vec::new();
        let mut ok = true;
        for p in pats {
            match self.pattern(d, scr, scr_e, p) {
                Ok((c, s, n)) => {
                    infos.push(ArmInfo { cond: c, narrow: n });
                    sets.push((s, p.span, matches!(p.kind, PatKind::Wild)));
                }
                Err(()) => {
                    ok = false;
                    infos.push(ArmInfo { cond: SigBit::ZERO, narrow: None });
                    sets.push((Set::Int(vec![]), p.span, false));
                }
            }
        }
        if !ok {
            return Err(());
        }
        let snip = d.snippet(scr_e.span);
        match &scr.ty {
            Ty::Enum(ei) => {
                let info = d.types.enums[*ei].clone();
                let n = info.variants.len();
                let mut covered = vec![false; n];
                for (s, span, wild) in &sets {
                    let mine: Vec<usize> = match s {
                        Set::All => (0..n).collect(),
                        Set::Variants(v) => v.clone(),
                        Set::Int(_) => unreachable!(),
                    };
                    let new: Vec<usize> = mine.iter().copied().filter(|&k| !covered[k]).collect();
                    if new.is_empty() {
                        d.emit(
                            Diagnostic::error(*span, "this arm can never match")
                                .with_label("already covered by earlier arms")
                                .with_help("remove it"),
                        );
                        ok = false;
                        continue;
                    }
                    if *wild && new.len() < n {
                        let names: Vec<String> =
                            new.iter().map(|&k| format!("`.{}`", info.variants[k].0)).collect();
                        d.emit(
                            Diagnostic::warning(*span, format!("`_` here stands for {}", names.join(", ")))
                                .with_label("catch-all arm")
                                .with_help("list the variants explicitly, so that a variant added later is not silently handled here"),
                        );
                    }
                    for k in new {
                        covered[k] = true;
                    }
                }
                let missing: Vec<String> = (0..n)
                    .filter(|&k| !covered[k])
                    .map(|k| format!("`.{}`", info.variants[k].0))
                    .collect();
                if !missing.is_empty() {
                    let s = if missing.len() == 1 { "" } else { "s" };
                    d.emit(
                        Diagnostic::error(
                            scr_e.span,
                            format!("`match` does not handle variant{s} {}", missing.join(", ")),
                        )
                        .with_label(format!("`{snip}` is a `{}`", info.name))
                        .with_help(format!("add an arm for {} (or `_ => ...`)", missing.join(", ")))
                        .with_secondary(info.span, format!("`{}` is defined here", info.name)),
                    );
                    ok = false;
                }
            }
            Ty::Bool | Ty::Int(_) => {
                let (w, lo, hi, signed) = match &scr.ty {
                    Ty::Bool => (1, BigInt::zero(), BigInt::from_i64(1), false),
                    Ty::Int(i) => (i.width(), i.lo.clone(), i.hi.clone(), i.signed()),
                    _ => unreachable!(),
                };
                if w > 127 {
                    if !sets.iter().any(|(s, _, _)| matches!(s, Set::All)) {
                        return d.err(
                            Diagnostic::error(
                                scr_e.span,
                                "cannot check exhaustiveness for values wider than 127 bits",
                            )
                            .with_label("too wide")
                            .with_help("add a `_ => ...` arm"),
                        );
                    }
                    return Ok(infos);
                }
                let full = interval_atoms(&lo, &hi, w);
                let mut seen: Vec<Atom> = Vec::new();
                for (s, span, _) in &sets {
                    let mine: Vec<Atom> = match s {
                        Set::All => full.clone(),
                        Set::Int(a) => a.clone(),
                        Set::Variants(_) => unreachable!(),
                    };
                    let prev: Vec<&Atom> = seen.iter().collect();
                    // Only the part inside the scrutinee's range matters.
                    let reachable = mine.iter().any(|a| {
                        full.iter().any(|f| {
                            let inside = intersect(a, f);
                            inside.is_some_and(|x| uncovered(&x, &prev, w).is_some())
                        })
                    });
                    if !reachable {
                        d.emit(
                            Diagnostic::error(*span, "this arm can never match")
                                .with_label("covered by earlier arms, or outside the values of the scrutinee")
                                .with_note(None, format!("`{snip}` has values {lo}..={hi}"))
                                .with_help("remove it"),
                        );
                        ok = false;
                    }
                    seen.extend(mine);
                }
                let prev: Vec<&Atom> = seen.iter().collect();
                for f in &full {
                    if let Some(v) = uncovered(f, &prev, w) {
                        let shown = if signed && w > 0 && (v >> (w - 1)) & 1 == 1 {
                            format!("{}", &BigInt::from_i128(v as i128) - &BigInt::pow2(w as u64))
                        } else if scr.ty == Ty::Bool {
                            (v == 1).to_string()
                        } else {
                            v.to_string()
                        };
                        d.emit(
                            Diagnostic::error(
                                scr_e.span,
                                format!("`match` is not exhaustive: `{snip}` may be {shown}"),
                            )
                            .with_label(format!("{shown} is not handled"))
                            .with_help("add an arm for the missing values, or `_ => ...`"),
                        );
                        ok = false;
                        break;
                    }
                }
            }
            t => {
                let tn = d.types.name(t);
                return d.err(
                    Diagnostic::error(scr_e.span, format!("cannot `match` on {tn}"))
                        .with_label(tn)
                        .with_help(
                            "match on integers, `bool` and enums; use `==` for structs and arrays",
                        ),
                );
            }
        }
        if ok { Ok(infos) } else { Err(()) }
    }

    fn pattern(
        &mut self,
        d: &mut Design<'a>,
        scr: &Hw,
        scr_e: &'a Expr,
        p: &'a Pattern,
    ) -> R<(SigBit, Set, Option<IntTy>)> {
        let snip = d.snippet(scr_e.span);
        let tn = d.types.name(&scr.ty);
        match &p.kind {
            PatKind::Wild => Ok((SigBit::ONE, Set::All, None)),
            PatKind::Or(alts) => {
                let mut cond = SigBit::ZERO;
                let mut atoms = Vec::new();
                let mut vars = Vec::new();
                let mut all = false;
                let mut narrow: Option<IntTy> = None;
                let mut narrow_ok = true;
                for a in alts {
                    let (c, s, n) = self.pattern(d, scr, scr_e, a)?;
                    cond = d.out.or1(cond, c);
                    match s {
                        Set::Int(x) => atoms.extend(x),
                        Set::Variants(v) => vars.extend(v),
                        Set::All => all = true,
                    }
                    match (n, &narrow) {
                        (Some(n), None) => narrow = Some(n),
                        (Some(n), Some(cur)) => {
                            let lo = if n.lo < cur.lo { n.lo.clone() } else { cur.lo.clone() };
                            let hi = if n.hi > cur.hi { n.hi.clone() } else { cur.hi.clone() };
                            narrow = Some(IntTy { lo, hi, decl: cur.decl.clone() });
                        }
                        (None, _) => narrow_ok = false,
                    }
                }
                let set = if all {
                    Set::All
                } else if matches!(scr.ty, Ty::Enum(_)) {
                    Set::Variants(vars)
                } else {
                    Set::Int(atoms)
                };
                Ok((cond, set, if narrow_ok { narrow } else { None }))
            }
            PatKind::Variant(ty, v) => {
                let Ty::Enum(ei) = scr.ty else {
                    return d.err(
                        Diagnostic::error(
                            p.span,
                            format!("`.{}` is an enum variant, but `{snip}` is {tn}", v.name),
                        )
                        .with_label("not an enum"),
                    );
                };
                let info = d.types.enums[ei].clone();
                if let Some(t) = ty
                    && t.name != info.name
                {
                    return d.err(
                        Diagnostic::error(
                            t.span,
                            format!("`{snip}` is a `{}`, not a `{}`", info.name, t.name),
                        )
                        .with_label("wrong enum"),
                    );
                }
                let Some(k) = info.variants.iter().position(|(n, _)| *n == v.name) else {
                    let mut dg = Diagnostic::error(
                        v.span,
                        format!("`{}` has no variant `{}`", info.name, v.name),
                    )
                    .with_label("unknown variant");
                    if let Some(s) = suggest(&v.name, info.variants.iter().map(|(n, _)| n.as_str()))
                    {
                        dg = dg.with_help(format!("did you mean `.{s}`?"));
                    }
                    return d.err(dg);
                };
                let code = info.variants[k].1.to_logic_vec(info.width as usize);
                let c = d.out.eq_const(scr.sig.clone(), &code);
                Ok((c, Set::Variants(vec![k]), None))
            }
            PatKind::Bool(b) => {
                if scr.ty != Ty::Bool {
                    return d.err(
                        Diagnostic::error(
                            p.span,
                            format!("`{b}` is a `bool` pattern, but `{snip}` is {tn}"),
                        )
                        .with_label("wrong type"),
                    );
                }
                let c = if *b { scr.sig.0[0] } else { d.out.not1(scr.sig.0[0]) };
                Ok((c, Set::Int(vec![Atom::Interval(*b as u128, *b as u128)]), None))
            }
            PatKind::Int(v) | PatKind::Range(v, _) => {
                let hi_v = match &p.kind {
                    PatKind::Range(_, h) => h.clone(),
                    _ => v.clone(),
                };
                let Ty::Int(it) = &scr.ty else {
                    let help = if scr.ty == Ty::Bool {
                        "match a `bool` with `true` and `false`"
                    } else {
                        "use variant patterns (`.Name`) or `_`"
                    };
                    return d.err(
                        Diagnostic::error(p.span, format!("a number cannot match {tn}"))
                            .with_label("wrong type")
                            .with_help(help),
                    );
                };
                if *v > hi_v {
                    return d.err(
                        Diagnostic::error(p.span, "empty range: the start is above the end")
                            .with_label("empty"),
                    );
                }
                let w = it.width();
                let lo = if *v > it.lo { v.clone() } else { it.lo.clone() };
                let hi = if hi_v < it.hi { hi_v.clone() } else { it.hi.clone() };
                if lo > hi {
                    return d.err(
                        Diagnostic::error(p.span, "this pattern can never match")
                            .with_label(format!("`{snip}` has values {}", it.range_text()))
                            .with_help("check the pattern's value against the scrutinee's type"),
                    );
                }
                let cond = if lo == hi {
                    let c = lo.to_logic_vec(w as usize);
                    d.out.eq_const(scr.sig.clone(), &c)
                } else {
                    let ww = w as usize + 1;
                    let x = scr.sig.extend(ww, it.signed());
                    let ge = d
                        .out
                        .binary(
                            BinaryOp::Ge,
                            true,
                            x.clone(),
                            Sig::from_const(&lo.to_logic_vec(ww)),
                        )
                        .0[0];
                    let le = d
                        .out
                        .binary(BinaryOp::Le, true, x, Sig::from_const(&hi.to_logic_vec(ww)))
                        .0[0];
                    d.out.and1(ge, le)
                };
                let atoms = if w <= 127 { interval_atoms(&lo, &hi, w) } else { vec![] };
                Ok((cond, Set::Int(atoms), Some(IntTy { lo, hi, decl: it.decl.clone() })))
            }
            PatKind::Bits(value, care, n) => {
                let Ty::Int(it) = &scr.ty else {
                    return d.err(
                        Diagnostic::error(p.span, format!("a bit pattern cannot match {tn}"))
                            .with_label("wrong type"),
                    );
                };
                let w = it.width();
                if *n != w {
                    return d.err(
                        Diagnostic::error(
                            p.span,
                            format!(
                                "this bit pattern has {n} digits, but `{snip}` is {w} bits wide"
                            ),
                        )
                        .with_label(format!("{n} digits"))
                        .with_help(format!(
                            "write exactly {w} digits, using `?` for bits that do not matter"
                        )),
                    );
                }
                let mask =
                    Sig::from_const(&BigInt::from_i128(*care as i128).to_logic_vec(w as usize));
                let val = BigInt::from_i128((*value & *care) as i128).to_logic_vec(w as usize);
                let masked = d.out.binary(BinaryOp::And, false, scr.sig.clone(), mask);
                let c = d.out.eq_const(masked, &val);
                Ok((c, Set::Int(vec![Atom::Cube { value: *value, care: *care }]), None))
            }
        }
    }
}

/// The bit-space atoms of the values `lo..=hi` in `w` bits.
fn interval_atoms(lo: &BigInt, hi: &BigInt, w: u32) -> Vec<Atom> {
    let m = |v: &BigInt| -> u128 {
        let mut x = 0u128;
        for i in 0..w as u64 {
            if v.bit(i) {
                x |= 1 << i;
            }
        }
        x
    };
    let (a, b) = (m(lo), m(hi));
    if lo.is_negative() && !hi.is_negative() {
        let top = if w == 128 { u128::MAX } else { (1u128 << w) - 1 };
        vec![Atom::Interval(a, top), Atom::Interval(0, b)]
    } else {
        vec![Atom::Interval(a, b)]
    }
}

fn intersect(a: &Atom, b: &Atom) -> Option<Atom> {
    match (a, b) {
        (Atom::Interval(x, y), Atom::Interval(p, q)) => {
            let (lo, hi) = ((*x).max(*p), (*y).min(*q));
            (lo <= hi).then_some(Atom::Interval(lo, hi))
        }
        // A cube inside an interval: keep the cube; `uncovered` intersects
        // with the full range anyway through the exhaustiveness pass.
        (c @ Atom::Cube { .. }, Atom::Interval(..))
        | (Atom::Interval(..), c @ Atom::Cube { .. }) => Some(c.clone()),
        (c @ Atom::Cube { .. }, Atom::Cube { .. }) => Some(c.clone()),
    }
}

impl<'a> ModCx<'a> {
    /// A `match` expression: the first matching arm's value.
    pub fn eval_match_expr(
        &mut self,
        d: &mut Design<'a>,
        s: &'a Expr,
        arms: &'a [(Pattern, Expr)],
        span: Span,
        exp: Option<&Ty>,
        rr: bool,
    ) -> R<Value> {
        let sh = self.eval_hw(d, s, None)?;
        let pats: Vec<&Pattern> = arms.iter().map(|(p, _)| p).collect();
        let infos = self.match_arms(d, &sh, s, &pats)?;
        let name = match &s.strip().kind {
            crate::ast::ExprKind::Ident(n) => Some(n.name.clone()),
            _ => None,
        };
        if sh.tags.is_static()
            && sh.sig.is_const()
            && let Some(k) = infos.iter().position(|i| i.cond == SigBit::ONE)
        {
            self.reg_read = rr;
            return self.eval(d, &arms[k].1, exp);
        }
        let mut vals = Vec::new();
        for (k, info) in infos.iter().enumerate() {
            let mut sc = Scope::default();
            if let (Some(n), Some(r)) = (&name, &info.narrow) {
                sc.narrow.insert(n.clone(), r.clone());
            }
            self.scopes.push(sc);
            self.reg_read = rr;
            let v = self.eval(d, &arms[k].1, exp);
            self.scopes.pop();
            vals.push(v?);
        }
        // Common type.
        let ty = match exp {
            Some(t) => t.clone(),
            None => {
                let mut hs = Vec::new();
                for (k, v) in vals.iter().enumerate() {
                    hs.push(self.to_hw(d, v.clone(), arms[k].1.span)?);
                }
                let mut t = hs[0].ty.clone();
                for (k, h) in hs.iter().enumerate().skip(1) {
                    let (a, b) = self.join_values(
                        d,
                        Value::Hw(Hw {
                            sig: Sig::xs(d.types.width(&t) as usize),
                            ty: t.clone(),
                            tags: Tags::none(),
                        }),
                        Value::Hw(h.clone()),
                        arms[0].1.span,
                        arms[k].1.span,
                        None,
                    )?;
                    let _ = b;
                    t = a.ty;
                }
                t
            }
        };
        let mut hs = Vec::new();
        for (k, v) in vals.into_iter().enumerate() {
            hs.push(self.coerce(d, v, &ty, arms[k].1.span, "this arm", Some(&arms[k].1))?);
        }
        let mut cur = hs.last().unwrap().clone();
        for k in (0..hs.len() - 1).rev() {
            let sig = d.out.mux(infos[k].cond, cur.sig.clone(), hs[k].sig.clone());
            cur = Hw { sig, ty: ty.clone(), tags: cur.tags.joined(&hs[k].tags) };
        }
        cur.tags.join(&sh.tags);
        let _ = span;
        Ok(Value::Hw(cur))
    }
}
