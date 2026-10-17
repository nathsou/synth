//! Lowering of procedural code: `always` blocks become multiplexers,
//! flip-flops, latches and memory write ports; `initial` blocks give
//! power-on values; functions and tasks are inlined.
//!
//! Execution is symbolic. Blocking assignments update the current value of
//! variable bits; non-blocking assignments are collected and take effect at
//! the end of the block. Both branches of a non-constant `if`/`case` are
//! executed from the same state and merged with multiplexers.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use synth_core::{Diagnostic, Logic, LogicVec, Span};
use synth_rtl::{CellKind, Clock, Control, Dff, Latch, MemId, MemWrite, Sig, SigBit, UnaryOp};
use synth_verilog::ast::{self, Expr, Stmt};

use crate::expr::Lvalue;
use crate::{Elab, Scope, Sym, VarInfo};

#[derive(Clone, Debug)]
pub(crate) struct PendingWrite {
    pub mem: MemId,
    pub addr: Sig,
    pub data: Sig,
    pub en: Sig,
}

/// Symbolic execution state of a process.
#[derive(Clone, Debug)]
pub(crate) struct ProcState {
    /// Current values of variable bits after blocking assignments.
    pub vals: HashMap<SigBit, SigBit>,
    /// Pending non-blocking assignments.
    pub nba: HashMap<SigBit, SigBit>,
    /// Assigned bits in first-assignment order (for deterministic output).
    pub order: Vec<SigBit>,
    seen: HashSet<SigBit>,
    /// Condition under which the current statement executes.
    pub cond: SigBit,
    pub writes: Vec<PendingWrite>,
    pub clocked: bool,
    /// Executing an `initial` block (constant interpretation).
    pub initial: bool,
    /// Errors that are reported even when the process is otherwise dropped.
    pub hard_errors: Vec<Diagnostic>,
}

impl ProcState {
    fn new(clocked: bool, initial: bool) -> Self {
        ProcState {
            vals: HashMap::new(),
            nba: HashMap::new(),
            order: Vec::new(),
            seen: HashSet::new(),
            cond: SigBit::ONE,
            writes: Vec::new(),
            clocked,
            initial,
            hard_errors: Vec::new(),
        }
    }

    pub fn current(&self, b: SigBit) -> SigBit {
        self.vals.get(&b).copied().unwrap_or(b)
    }

    fn note(&mut self, b: SigBit) {
        if self.seen.insert(b) {
            self.order.push(b);
        }
    }
}

/// Splits an expression like `rst`, `!rst_n`, `~rst_n`, `rst_n == 0` into
/// the tested signal and whether it is negated.
fn split_condition(e: &Expr) -> Option<(&Expr, bool)> {
    match e {
        Expr::Unary { op: ast::UnaryOp::LogicNot | ast::UnaryOp::BitNot, expr, .. } => {
            split_condition(expr).map(|(s, n)| (s, !n))
        }
        Expr::Binary {
            op:
                op @ (ast::BinaryOp::Eq
                | ast::BinaryOp::Ne
                | ast::BinaryOp::CaseEq
                | ast::BinaryOp::CaseNe),
            lhs,
            rhs,
            ..
        } => {
            let val = match rhs.as_ref() {
                Expr::Number(n, _) => crate::expr::number_value(n)?.0.to_u64()?,
                _ => return None,
            };
            let ne = matches!(op, ast::BinaryOp::Ne | ast::BinaryOp::CaseNe);
            let negated = (val == 0) != ne;
            split_condition(lhs).map(|(s, n)| (s, n != negated))
        }
        Expr::Ident(..) | Expr::Index { .. } => Some((e, false)),
        _ => None,
    }
}

impl<'a> Elab<'a> {
    // ---------------- always ----------------

    pub(crate) fn always(&mut self, body: &'a Stmt, span: Span) {
        let Stmt::Timing { control, body: inner, .. } = body else {
            self.warn_once(span, "`always` without an event control is not synthesizable; ignored");
            return;
        };
        let events = match control {
            ast::EventControl::Star => None,
            ast::EventControl::List(l) => Some(l),
        };
        let edges: Vec<&ast::EventExpr> = events
            .map(|l| l.iter().filter(|e| e.edge != ast::Edge::Any).collect())
            .unwrap_or_default();
        let any_level = events.is_some_and(|l| l.iter().any(|e| e.edge == ast::Edge::Any));
        if !edges.is_empty() && any_level {
            self.error(span, "mixed edge and level sensitivity is not synthesizable");
            return;
        }
        if edges.is_empty() {
            self.comb_process(inner);
        } else {
            self.clocked_process(&edges, inner, span);
        }
    }

    fn event_bit(&mut self, e: &Expr) -> SigBit {
        let (w, s) = self.expr_type(e);
        let sig = self.emit(e, w, s);
        if sig.width() != 1 {
            self.warn_once(e.span(), "multi-bit event expression; using bit 0");
        }
        sig.0[0]
    }

    fn comb_process(&mut self, body: &'a Stmt) {
        let first_cell = self.out.cells.len();
        self.proc = Some(ProcState::new(false, false));
        self.exec(body);
        let st = self.proc.take().unwrap();
        if !st.writes.is_empty() {
            self.error(body.span(), "memory writes need a clocked process");
        }
        // Per-bit mux structure created by this process, for latch analysis.
        let mut muxes: HashMap<SigBit, (SigBit, SigBit, SigBit)> = HashMap::new();
        for ci in first_cell..self.out.cells.len() {
            if let CellKind::Mux { a, b, s, y, .. } =
                &self.out.cells[synth_rtl::CellId::new(ci)].kind
            {
                for i in 0..y.width() {
                    muxes.insert(y.0[i], (*s, a.0[i], b.0[i]));
                }
            }
        }
        let mut latch_bits: Vec<(SigBit, SigBit, SigBit)> = Vec::new();
        for &k in &st.order {
            let fin = st.nba.get(&k).or(st.vals.get(&k)).copied().unwrap_or(k);
            if fin == k {
                continue;
            }
            let mut memo = HashMap::new();
            let (data, en) = self.latch_split(fin, k, &muxes, &mut memo);
            if en == SigBit::ONE {
                self.out.connect(Sig::bit(k), Sig::bit(fin));
            } else {
                latch_bits.push((k, data, en));
            }
        }
        // Group latch bits by enable.
        let mut by_en: Vec<(SigBit, Vec<(SigBit, SigBit)>)> = Vec::new();
        for (k, d, en) in latch_bits {
            match by_en.iter_mut().find(|(e, _)| *e == en) {
                Some((_, v)) => v.push((k, d)),
                None => by_en.push((en, vec![(k, d)])),
            }
        }
        for (en, bits) in by_en {
            let names: Vec<String> =
                bits.iter().take(3).map(|(k, _)| self.out.bit_name(*k)).collect();
            self.warn_once(body.span(), format!("latch inferred for {}", names.join(", ")));
            let q: Sig = bits.iter().map(|b| b.0).collect();
            let d: Sig = bits.iter().map(|b| b.1).collect();
            let name = self.fresh("latch");
            self.out.add_cell(
                name,
                CellKind::Latch(Box::new(Latch { en: Control { sig: en, pol: true }, d, q })),
            );
        }
    }

    /// Decomposes the final value of `k` into data and enable when it
    /// depends on `k` itself through this process's multiplexers.
    fn latch_split(
        &mut self,
        v: SigBit,
        k: SigBit,
        muxes: &HashMap<SigBit, (SigBit, SigBit, SigBit)>,
        memo: &mut HashMap<SigBit, (SigBit, SigBit)>,
    ) -> (SigBit, SigBit) {
        if v == k {
            return (SigBit::X, SigBit::ZERO);
        }
        if let Some(&r) = memo.get(&v) {
            return r;
        }
        let r = match muxes.get(&v) {
            Some(&(s, a, b)) => {
                let (da, ea) = self.latch_split(a, k, muxes, memo);
                let (db, eb) = self.latch_split(b, k, muxes, memo);
                if ea == SigBit::ONE && eb == SigBit::ONE {
                    (v, SigBit::ONE)
                } else {
                    let d = self.out.mux(s, Sig::bit(da), Sig::bit(db)).0[0];
                    let e = self.out.mux(s, Sig::bit(ea), Sig::bit(eb)).0[0];
                    (d, e)
                }
            }
            None => (v, SigBit::ONE),
        };
        memo.insert(v, r);
        r
    }

    fn clocked_process(&mut self, edges: &[&'a ast::EventExpr], body: &'a Stmt, span: Span) {
        let bits: Vec<(SigBit, bool)> =
            edges.iter().map(|e| (self.event_bit(&e.expr), e.edge == ast::Edge::Posedge)).collect();
        if bits.len() == 1 {
            let clk = Clock { sig: bits[0].0, posedge: bits[0].1 };
            self.proc = Some(ProcState::new(true, false));
            self.exec(body);
            let st = self.proc.take().unwrap();
            self.commit_clocked(&st, clk, None, None);
            return;
        }
        // Asynchronous reset: `if (rst) <reset> else <logic>`, possibly inside
        // blocks that declare local variables.
        let mut body = body;
        let mut scopes = 0;
        while let Stmt::Block { name, decls, stmts, .. } = body
            && stmts.len() == 1
        {
            scopes += usize::from(self.enter_block(name, decls));
            body = &stmts[0];
        }
        self.async_process(&bits, body, span);
        for _ in 0..scopes {
            self.scopes.pop();
        }
    }

    fn async_process(&mut self, bits: &[(SigBit, bool)], body: &'a Stmt, span: Span) {
        let Stmt::If { cond, then, otherwise, .. } = body else {
            self.error(span, "processes with several edges must start with `if (<async reset>)`");
            return;
        };
        let Some((sig_expr, negated)) = split_condition(cond) else {
            self.error(cond.span(), "unsupported asynchronous reset condition");
            return;
        };
        let rbit = self.event_bit(sig_expr);
        let Some(ri) = bits.iter().position(|(b, _)| *b == rbit) else {
            self.error(cond.span(), "asynchronous reset condition does not test an edge signal");
            return;
        };
        if bits.len() != 2 {
            self.error(span, "only one asynchronous reset per process is supported");
            return;
        }
        let active_high = !negated;
        if active_high != bits[ri].1 {
            self.error(cond.span(), "asynchronous reset polarity does not match its edge");
            return;
        }
        let clk_i = 1 - ri;
        let clk = Clock { sig: bits[clk_i].0, posedge: bits[clk_i].1 };
        let rst = Control { sig: rbit, pol: active_high };
        // Reset branch: constant values.
        self.proc = Some(ProcState::new(true, false));
        self.exec(then);
        let rst_state = self.proc.take().unwrap();
        // Normal branch.
        self.proc = Some(ProcState::new(true, false));
        if let Some(o) = otherwise {
            self.exec(o);
        }
        let st = self.proc.take().unwrap();
        self.commit_clocked(&st, clk, Some(rst), Some(&rst_state));
    }

    fn commit_clocked(
        &mut self,
        st: &ProcState,
        clk: Clock,
        rst: Option<Control>,
        rst_state: Option<&ProcState>,
    ) {
        // Reset values by bit.
        let mut reset_val: HashMap<SigBit, Logic> = HashMap::new();
        if let Some(rs) = rst_state {
            for &k in &rs.order {
                let v = rs.nba.get(&k).or(rs.vals.get(&k)).copied().unwrap_or(k);
                match v {
                    SigBit::Const(l) => {
                        reset_val.insert(k, l);
                    }
                    _ => {
                        let n = self.out.bit_name(k);
                        self.diags.error(
                            Span::default(),
                            format!("asynchronous reset value of `{n}` is not constant"),
                        );
                    }
                }
            }
            if !rs.writes.is_empty() {
                self.diags.error(Span::default(), "memory write in an asynchronous reset branch");
            }
        }
        let mut all: Vec<SigBit> = st.order.clone();
        if let Some(rs) = rst_state {
            for &k in &rs.order {
                if !st.seen.contains(&k) {
                    all.push(k);
                }
            }
        }
        // Group bits by (wire, has reset).
        let mut groups: Vec<((synth_rtl::WireId, bool), Vec<SigBit>)> = Vec::new();
        for k in all {
            let SigBit::Wire(w, _) = k else { continue };
            let key = (w, reset_val.contains_key(&k));
            match groups.iter_mut().find(|(g, _)| *g == key) {
                Some((_, v)) => v.push(k),
                None => groups.push((key, vec![k])),
            }
        }
        for ((w, has_rst), mut ks) in groups {
            ks.sort();
            let mut d = Sig::new();
            for &k in &ks {
                let next = st.nba.get(&k).or(st.vals.get(&k)).copied().unwrap_or(k);
                let next = match rst {
                    // Bits without a reset value hold while reset is active.
                    Some(r) if !has_rst => {
                        let active = if r.pol { r.sig } else { self.out.not1(r.sig) };
                        self.out.mux(active, Sig::bit(next), Sig::bit(k)).0[0]
                    }
                    _ => next,
                };
                d.push(next);
            }
            let init: Vec<Logic> =
                ks.iter().map(|k| self.init.get(k).copied().unwrap_or(Logic::X)).collect();
            let arst = if has_rst {
                let v: Vec<Logic> = ks.iter().map(|k| reset_val[k]).collect();
                Some((rst.unwrap(), LogicVec::from_bits(&v)))
            } else {
                None
            };
            let dff = Dff {
                clk,
                d,
                q: Sig(ks.clone()),
                en: None,
                srst: None,
                en_over_srst: false,
                arst,
                init: LogicVec::from_bits(&init),
            };
            let name = format!("{}$dff", self.out.wires[w].name);
            self.out.add_cell(name, CellKind::Dff(Box::new(dff)));
        }
        for wr in &st.writes {
            self.out.memories[wr.mem].writes.push(MemWrite {
                clk,
                en: wr.en.clone(),
                addr: wr.addr.clone(),
                data: wr.data.clone(),
            });
        }
    }

    // ---------------- initial ----------------

    pub(crate) fn initial(&mut self, body: &'a Stmt) {
        let before = self.diags.items.len();
        self.proc = Some(ProcState::new(false, true));
        self.exec(body);
        let st = self.proc.take().unwrap();
        if self.diags.items[before..].iter().any(|d| d.severity == synth_core::Severity::Error) {
            self.diags.items.truncate(before);
            self.warn_once(body.span(), "`initial` block is not synthesizable and was ignored");
        }
        self.diags.items.extend(st.hard_errors);
        if self.diags.items.len() > before {
            return;
        }
        for &k in &st.order {
            let v = st.vals.get(&k).or(st.nba.get(&k)).copied().unwrap_or(k);
            match v {
                SigBit::Const(l) => {
                    self.init.insert(k, l);
                }
                _ => self.warn_once(body.span(), "non-constant value in `initial` block ignored"),
            }
        }
    }

    // ---------------- statements ----------------

    /// Opens the scope of a named block or one with declarations; returns
    /// whether a scope was pushed.
    fn enter_block(&mut self, name: &Option<ast::Ident>, decls: &'a [ast::Item]) -> bool {
        let prefix = match name {
            Some(n) => format!("{}{}.", self.prefix(), n.name),
            None => self.prefix(),
        };
        let scoped = name.is_some() || !decls.is_empty();
        if scoped {
            self.scopes.push(Scope { names: HashMap::new(), prefix });
            for d in decls {
                match d {
                    ast::Item::Var(v) => self.local_vars(v),
                    ast::Item::Param(p) => self.param_decl(p, &[]),
                    _ => {}
                }
            }
        }
        scoped
    }

    pub(crate) fn exec(&mut self, s: &'a Stmt) {
        match s {
            Stmt::Null(_) => {}
            Stmt::Block { name, decls, stmts, .. } => {
                let scoped = self.enter_block(name, decls);
                for st in stmts {
                    self.exec(st);
                }
                if scoped {
                    self.scopes.pop();
                }
            }
            Stmt::Assign { lhs, rhs, nonblocking, .. } => {
                let Some(lv) = self.lvalue(lhs, false) else { return };
                let lw = lv.width() as u32;
                let (rw, rs) = self.expr_type(rhs);
                let v = self.emit(rhs, lw.max(rw), rs);
                let v = Sig(v.0[..lw as usize].to_vec());
                self.assign(lv, v, *nonblocking, lhs.span());
            }
            Stmt::If { cond, then, otherwise, .. } => {
                let (w, sg) = self.expr_type(cond);
                let c = self.emit(cond, w, sg);
                let c = self.out.reduce_bool(c);
                self.branch(
                    c,
                    |e| e.exec(then),
                    |e| {
                        if let Some(o) = otherwise {
                            e.exec(o)
                        }
                    },
                );
            }
            Stmt::Case { kind, expr, items, attrs, .. } => {
                let full = attrs.iter().any(|a| a.name.name == "full_case");
                self.case(*kind, expr, items, full);
            }
            Stmt::For { init, cond, step, body, span } => {
                self.exec(init);
                let mut n = 0;
                loop {
                    let Some(go) = self.loop_cond(cond, *span) else { return };
                    if !go {
                        break;
                    }
                    self.exec(body);
                    self.exec(step);
                    n += 1;
                    if n > 100_000 {
                        self.error(*span, "loop does not terminate (100000 iterations)");
                        return;
                    }
                }
            }
            Stmt::While { cond, body, span } => {
                let mut n = 0;
                loop {
                    let Some(go) = self.loop_cond(cond, *span) else { return };
                    if !go {
                        break;
                    }
                    self.exec(body);
                    n += 1;
                    if n > 100_000 {
                        self.error(*span, "loop does not terminate (100000 iterations)");
                        return;
                    }
                }
            }
            Stmt::Repeat { count, body, .. } => {
                let Some(n) = self.const_int(count) else { return };
                for _ in 0..n.max(0) {
                    self.exec(body);
                }
            }
            Stmt::Forever { span, .. } => self.error(*span, "`forever` is not synthesizable"),
            Stmt::Timing { span, .. } => {
                self.error(*span, "nested event controls are not synthesizable")
            }
            Stmt::Delay { body, .. } => self.exec(body),
            Stmt::TaskCall { name, args, span } => match self.lookup(&name.name).cloned() {
                Some(Sym::Task(t)) => self.call_task(t, args, *span),
                _ => self.error(name.span, format!("`{}` is not a task", name.name)),
            },
            Stmt::SysTask { name, args, span } => self.sys_task(&name.name, args, *span),
            Stmt::Disable { span, .. } => self.error(*span, "`disable` is not supported"),
        }
    }

    fn loop_cond(&mut self, cond: &'a Expr, span: Span) -> Option<bool> {
        let (w, s) = self.expr_type(cond);
        let c = self.emit(cond, w, s);
        match c.as_const().and_then(|v| v.truthy()) {
            Some(b) => Some(b),
            None => {
                self.error(span, "loop condition must be constant at elaboration time");
                None
            }
        }
    }

    /// Executes `then`/`otherwise` under condition `c` and merges the states.
    fn branch(
        &mut self,
        c: SigBit,
        then: impl FnOnce(&mut Self),
        otherwise: impl FnOnce(&mut Self),
    ) {
        match c {
            SigBit::Const(Logic::One) => return then(self),
            SigBit::Const(Logic::Zero) => return otherwise(self),
            _ => {}
        }
        let Some(base) = self.proc.take() else {
            then(self);
            return;
        };
        if base.initial {
            // In initial blocks, unknown conditions cannot be resolved.
            self.proc = Some(base);
            self.diags.error(Span::default(), "non-constant condition in an `initial` block");
            return;
        }
        let nc = self.out.not1(c);
        let mut t = base.clone();
        t.cond = self.out.and1(base.cond, c);
        self.proc = Some(t);
        then(self);
        let t = self.proc.take().unwrap();
        let mut e = base.clone();
        e.cond = self.out.and1(base.cond, nc);
        self.proc = Some(e);
        otherwise(self);
        let e = self.proc.take().unwrap();
        self.proc = Some(self.merge(c, t, e, &base));
    }

    fn merge(&mut self, c: SigBit, t: ProcState, e: ProcState, base: &ProcState) -> ProcState {
        let mut out = base.clone();
        let mut keys: Vec<SigBit> = t.order.clone();
        for &k in &e.order {
            if !t.seen.contains(&k) {
                keys.push(k);
            }
        }
        for &k in &keys {
            out.note(k);
            let in_t = t.vals.contains_key(&k);
            let in_e = e.vals.contains_key(&k);
            if in_t || in_e {
                let tv = t.vals.get(&k).copied().unwrap_or(k);
                let ev = e.vals.get(&k).copied().unwrap_or(k);
                let v =
                    if tv == ev { tv } else { self.out.mux(c, Sig::bit(ev), Sig::bit(tv)).0[0] };
                out.vals.insert(k, v);
            }
            let in_t = t.nba.contains_key(&k);
            let in_e = e.nba.contains_key(&k);
            if in_t || in_e {
                let tv = t.nba.get(&k).copied().unwrap_or(k);
                let ev = e.nba.get(&k).copied().unwrap_or(k);
                let v =
                    if tv == ev { tv } else { self.out.mux(c, Sig::bit(ev), Sig::bit(tv)).0[0] };
                out.nba.insert(k, v);
            }
        }
        out.writes = t.writes;
        out.writes.extend(e.writes[base.writes.len()..].iter().cloned());
        out
    }

    fn assign(&mut self, lv: Lvalue, v: Sig, nonblocking: bool, span: Span) {
        let Some(st) = self.proc.as_ref() else { return };
        let clocked = st.clocked;
        let initial = st.initial;
        match lv {
            Lvalue::Bits(bits) => {
                let st = self.proc.as_mut().unwrap();
                for (k, val) in bits.into_iter().zip(v.0) {
                    st.note(k);
                    if nonblocking {
                        st.nba.insert(k, val);
                    } else {
                        st.vals.insert(k, val);
                    }
                }
            }
            Lvalue::Dynamic { base, index, index_signed, offset, width } => {
                let n = base.len();
                let mut new_vals = Vec::new();
                for (q, &bk) in base.iter().enumerate() {
                    let st = self.proc.as_ref().unwrap();
                    let old = if nonblocking {
                        st.nba.get(&bk).copied().unwrap_or(bk)
                    } else {
                        st.current(bk)
                    };
                    let mut val = old;
                    for k in 0..width {
                        if k > q || q - k + width > n {
                            continue;
                        }
                        let p = (q - k) as i64;
                        // Index value selecting position p: p - offset.
                        let target = p - offset;
                        let iw = index.width();
                        let tv = LogicVec::from_i64(target, iw.max(64));
                        if tv.resize(iw, index_signed).resize(iw.max(64), index_signed) != tv {
                            continue; // not representable in the index width
                        }
                        let hit = self.out.eq_const(index.clone(), &tv.resize(iw, false));
                        val = self.out.mux(hit, Sig::bit(val), Sig::bit(v.0[k])).0[0];
                    }
                    new_vals.push((bk, val));
                }
                let st = self.proc.as_mut().unwrap();
                for (k, val) in new_vals {
                    if val == k {
                        continue;
                    }
                    st.note(k);
                    if nonblocking {
                        st.nba.insert(k, val);
                    } else {
                        st.vals.insert(k, val);
                    }
                }
            }
            Lvalue::ArrayDyn { elems, indices, index, index_signed } => {
                let iw = index.width();
                let mut new_vals = Vec::new();
                for (bits, &i) in elems.iter().zip(&indices) {
                    let tv = LogicVec::from_i64(i, iw.max(64));
                    if tv.resize(iw, index_signed).resize(iw.max(64), index_signed) != tv {
                        continue; // not representable in the index width
                    }
                    let hit = self.out.eq_const(index.clone(), &tv.resize(iw, false));
                    for (j, &bk) in bits.iter().enumerate() {
                        let st = self.proc.as_ref().unwrap();
                        let old = if nonblocking {
                            st.nba.get(&bk).copied().unwrap_or(bk)
                        } else {
                            st.current(bk)
                        };
                        let val = self.out.mux(hit, Sig::bit(old), Sig::bit(v.0[j])).0[0];
                        new_vals.push((bk, val));
                    }
                }
                let st = self.proc.as_mut().unwrap();
                for (k, val) in new_vals {
                    if val == k {
                        continue;
                    }
                    st.note(k);
                    if nonblocking {
                        st.nba.insert(k, val);
                    } else {
                        st.vals.insert(k, val);
                    }
                }
            }
            Lvalue::Mem { mem, addr, lo, width } => {
                if initial {
                    let (Some(a), Some(d)) = (addr.as_const(), v.as_const()) else {
                        self.error(span, "memory initialization needs constant address and data");
                        return;
                    };
                    let Some(a) = a.to_i64(false) else { return };
                    let m = &mut self.out.memories[mem.mem];
                    let idx = a - m.offset;
                    if idx < 0 || idx >= m.depth as i64 {
                        return;
                    }
                    let word = &mut m.init[idx as usize];
                    for i in 0..width as usize {
                        word.set(lo as usize + i, d.get(i));
                    }
                    return;
                }
                if !clocked {
                    self.error(span, "memory writes need a clocked process");
                    return;
                }
                let st = self.proc.as_ref().unwrap();
                let cond = st.cond;
                let mw = mem.width as usize;
                let mut data = Sig::xs(mw);
                let mut en = Sig::zeros(mw);
                for i in 0..width as usize {
                    data.0[lo as usize + i] = v.0[i];
                    en.0[lo as usize + i] = cond;
                }
                self.proc.as_mut().unwrap().writes.push(PendingWrite {
                    mem: mem.mem,
                    addr,
                    data,
                    en,
                });
            }
        }
    }

    fn case(
        &mut self,
        kind: ast::CaseKind,
        subject: &'a Expr,
        items: &'a [ast::CaseItem],
        full_case: bool,
    ) {
        // All items and the subject are sized to the widest of them.
        let (mut w, mut signed) = self.expr_type(subject);
        for it in items {
            for e in &it.exprs {
                let (ew, es) = self.expr_type(e);
                w = w.max(ew);
                signed &= es;
            }
        }
        let subj = self.emit(subject, w, signed);
        let mut conds: Vec<SigBit> = Vec::new();
        for it in items {
            let mut c = SigBit::ZERO;
            for e in &it.exprs {
                let hit = self.case_match(kind, &subj, e, w, signed);
                c = self.out.or1(c, hit);
            }
            conds.push(c);
        }
        // Without a default, a case that covers every value (or is marked
        // full_case, meaning other values do not matter) runs its last item
        // that can match unconditionally instead of keeping old values (no
        // latch). Items that can never match stay out of the logic.
        let has_default = items.iter().any(|it| it.exprs.is_empty());
        if !has_default
            && (full_case || self.case_covers_all(kind, w, items))
            && let Some(last) = (0..items.len())
                .rev()
                .find(|&i| !items[i].exprs.is_empty() && conds[i] != SigBit::ZERO)
        {
            conds[last] = SigBit::ONE;
        }
        self.case_chain(items, &conds, 0);
    }

    /// Whether constant case items cover all values of a `w`-bit subject.
    fn case_covers_all(&mut self, kind: ast::CaseKind, w: u32, items: &'a [ast::CaseItem]) -> bool {
        if w > 12 || kind != ast::CaseKind::Case {
            return false;
        }
        let mut seen = vec![false; 1 << w];
        for it in items {
            for e in &it.exprs {
                let v = self.emit(e, w, false);
                match v.as_const().and_then(|c| c.to_u64()) {
                    Some(x) if (x as usize) < seen.len() => seen[x as usize] = true,
                    _ => return false,
                }
            }
        }
        seen.iter().all(|&b| b)
    }

    fn case_chain(&mut self, items: &'a [ast::CaseItem], conds: &[SigBit], i: usize) {
        // Skip the default item in the chain; it runs when nothing matched.
        let mut i = i;
        while i < items.len() && items[i].exprs.is_empty() {
            i += 1;
        }
        if i >= items.len() {
            if let Some(d) = items.iter().find(|it| it.exprs.is_empty()) {
                self.exec(&d.body);
            }
            return;
        }
        let c = conds[i];
        self.branch(c, |e| e.exec(&items[i].body), |e| e.case_chain(items, conds, i + 1));
    }

    fn case_match(
        &mut self,
        kind: ast::CaseKind,
        subj: &Sig,
        item: &'a Expr,
        w: u32,
        signed: bool,
    ) -> SigBit {
        let iv = self.emit(item, w, signed);
        if kind == ast::CaseKind::Case || !iv.is_const() {
            if let Some(c) = iv.as_const()
                && !c.is_known()
            {
                // A case item with x/z never matches a real signal.
                return SigBit::ZERO;
            }
            return self.out.binary(synth_rtl::BinaryOp::Eq, false, subj.clone(), iv).0[0];
        }
        // casez: z/? bits are wildcards; casex: x and z bits are.
        let c = iv.as_const().unwrap();
        let mut a = Sig::new();
        let mut b = Sig::new();
        for i in 0..w as usize {
            let bit = c.get(i);
            let wild = match kind {
                ast::CaseKind::Casez => bit == Logic::Z,
                _ => !bit.is_known(),
            };
            if !wild {
                a.push(subj.0[i]);
                b.push(SigBit::Const(bit));
            }
        }
        if a.is_empty() {
            return SigBit::ONE;
        }
        self.out.binary(synth_rtl::BinaryOp::Eq, false, a, b).0[0]
    }

    fn sys_task(&mut self, name: &str, args: &'a [Expr], span: Span) {
        match name {
            "display" | "write" | "strobe" | "monitor" | "finish" | "stop" | "fatal" | "error"
            | "warning" | "info" | "dumpfile" | "dumpvars" | "fflush" | "fdisplay" | "fwrite"
            | "timeformat" => {}
            "readmemh" | "readmemb" => {
                if !self.proc.as_ref().is_some_and(|p| p.initial) {
                    self.error(span, format!("${name} is only supported in `initial` blocks"));
                    return;
                }
                self.readmem(name == "readmemh", args, span);
            }
            _ => self.warn_once(span, format!("system task ${name} ignored")),
        }
    }

    fn readmem(&mut self, hex: bool, args: &'a [Expr], span: Span) {
        let (Some(fexpr), Some(target)) = (args.first(), args.get(1)) else {
            self.error(span, "expected $readmem(\"file\", memory[, start[, end]])");
            return;
        };
        let file = match fexpr {
            Expr::Str(f, _) => f.clone(),
            // A string-valued parameter: bytes from the most significant end.
            e => {
                let Some((v, _)) = self.const_expr(e) else { return };
                let Some(f) = const_string(&v) else {
                    self.error(e.span(), "memory file name must be a string");
                    return;
                };
                f
            }
        };
        let file = file.as_str();
        let Expr::Ident(parts, tspan) = target else {
            self.error(target.span(), "expected a memory");
            return;
        };
        let Some(Sym::Mem(m)) = self.lookup_path(parts) else {
            self.error(*tspan, "expected a memory");
            return;
        };
        let start = match args.get(2) {
            Some(e) => match self.const_int(e) {
                Some(v) => v,
                None => return,
            },
            None => m.low(),
        };
        // Resolve relative to the source file, then the working directory.
        let src_dir = Path::new(&self.sm.file(span.file).name).parent().map(Path::to_path_buf);
        let candidates: Vec<std::path::PathBuf> = src_dir
            .iter()
            .map(|d| d.join(file))
            .chain(std::iter::once(Path::new(file).to_path_buf()))
            .collect();
        let Some(text) = candidates.iter().find_map(|p| std::fs::read_to_string(p).ok()) else {
            self.hard_error(span, format!("cannot read memory file \"{file}\""));
            return;
        };
        let words = match read_mem_file(&text, hex, m.width as usize) {
            Ok(w) => w,
            Err(e) => {
                self.hard_error(span, format!("{file}: {e}"));
                return;
            }
        };
        let mem = &mut self.out.memories[m.mem];
        let mut addr = start;
        for (explicit, word) in words {
            if let Some(a) = explicit {
                addr = a as i64;
            }
            let idx = addr - mem.offset;
            if idx >= 0 && idx < mem.depth as i64 {
                mem.init[idx as usize] = word;
            }
            addr += 1;
        }
    }

    // ---------------- functions and tasks ----------------

    /// Declares local variables of a named block or subroutine as hidden
    /// wires, initially `x` within the current process.
    fn local_vars(&mut self, v: &ast::VarDecl) {
        let Some((msb, lsb)) = self.range(&v.range) else { return };
        let (width, signed, msb, lsb) = match v.kind {
            ast::NetKind::Integer => (32, true, 31, 0),
            _ => ((msb - lsb).unsigned_abs() as u32 + 1, v.signed, msb, lsb),
        };
        for d in &v.names {
            if !d.dims.is_empty() {
                self.error(d.name.span, "local memories are not supported");
                continue;
            }
            let name = format!("{}{}", self.prefix(), d.name.name);
            let w = self.new_wire(&name, width, signed);
            self.declare(
                &d.name.name,
                Sym::Var(VarInfo { wire: w, width, signed, msb, lsb, is_var: true }),
                d.name.span,
            );
            if let Some(e) = &d.init {
                let lhs = Expr::Ident(vec![(d.name.clone(), None)], d.name.span);
                let _ = lhs;
                let (rw, rs) = self.expr_type(e);
                let val = self.emit(e, width.max(rw), rs);
                if let Some(st) = self.proc.as_mut() {
                    for i in 0..width {
                        let k = SigBit::Wire(w, i);
                        st.note(k);
                        st.vals.insert(k, val.0[i as usize]);
                    }
                }
            }
        }
    }

    fn declare_local(
        &mut self,
        name: &ast::Ident,
        width: u32,
        signed: bool,
        msb: i64,
        lsb: i64,
    ) -> VarInfo {
        let wname = format!("{}{}", self.prefix(), name.name);
        let w = self.new_wire(&wname, width, signed);
        let v = VarInfo { wire: w, width, signed, msb, lsb, is_var: true };
        self.declare(&name.name, Sym::Var(v.clone()), name.span);
        v
    }

    /// Drops the process state of a returning subroutine's variables (wires
    /// created since `first`): their values were consumed by the caller and
    /// they must not become process outputs (or latches).
    fn forget_locals(&mut self, first: usize) {
        let Some(st) = self.proc.as_mut() else { return };
        let local = |b: &SigBit| matches!(b, SigBit::Wire(w, _) if w.index() >= first);
        st.vals.retain(|k, _| !local(k));
        st.nba.retain(|k, _| !local(k));
        st.seen.retain(|k| !local(k));
        st.order.retain(|k| !local(k));
    }

    fn set_local(&mut self, v: &VarInfo, val: &Sig) {
        if let Some(st) = self.proc.as_mut() {
            for i in 0..v.width {
                let k = SigBit::Wire(v.wire, i);
                st.vals.insert(k, val.0.get(i as usize).copied().unwrap_or(SigBit::X));
            }
        }
    }

    pub(crate) fn call_function(
        &mut self,
        f: &'a ast::SubroutineDecl,
        args: &[Expr],
        span: Span,
    ) -> Sig {
        if self.depth > 64 {
            self.error(span, "function recursion too deep");
            return Sig::xs(1);
        }
        let ports: Vec<(&ast::PortDecl, &ast::Ident)> =
            f.ports.iter().flat_map(|p| p.names.iter().map(move |(n, _)| (p, n))).collect();
        if args.len() != ports.len() {
            self.error(
                span,
                format!(
                    "function `{}` takes {} arguments, {} given",
                    f.name.name,
                    ports.len(),
                    args.len()
                ),
            );
            return Sig::xs(1);
        }
        // Evaluate arguments in the caller's scope first.
        let mut values = Vec::new();
        for (i, (p, _)) in ports.iter().enumerate() {
            let (msb, lsb) = self.range(&p.range).unwrap_or((0, 0));
            let (w, s) = match p.kind {
                Some(ast::NetKind::Integer) => (32, true),
                _ => ((msb - lsb).unsigned_abs() as u32 + 1, p.signed),
            };
            let (aw, asg) = self.expr_type(&args[i]);
            let v = self.emit(&args[i], w.max(aw), asg);
            values.push((Sig(v.0[..w as usize].to_vec()), w, s, msb, lsb));
        }
        let temp_proc = self.proc.is_none();
        if temp_proc {
            self.proc = Some(ProcState::new(false, false));
        }
        self.depth += 1;
        let first_local = self.out.wires.len();
        let prefix = format!("{}.", self.fresh(&f.name.name));
        self.scopes.push(Scope { names: HashMap::new(), prefix });
        for d in &f.decls {
            if let ast::Item::Param(p) = d {
                self.param_decl(p, &[]);
            }
        }
        for ((_, n), (v, w, s, msb, lsb)) in ports.iter().zip(values) {
            let info = self.declare_local(n, w, s, msb, lsb);
            self.set_local(&info, &v);
        }
        let (rw, rs, rmsb, rlsb) = match f.ret_kind {
            Some(ast::NetKind::Integer) => (32, true, 31, 0),
            _ => {
                let (m, l) = self.range(&f.range).unwrap_or((0, 0));
                ((m - l).unsigned_abs() as u32 + 1, f.signed, m, l)
            }
        };
        let ret = self.declare_local(&f.name, rw, rs, rmsb, rlsb);
        self.set_local(&ret, &Sig::xs(rw as usize));
        for d in &f.decls {
            if let ast::Item::Var(v) = d {
                self.local_vars(v);
                for n in &v.names {
                    if let Some(Sym::Var(info)) = self.lookup(&n.name.name).cloned()
                        && n.init.is_none()
                    {
                        self.set_local(&info, &Sig::xs(info.width as usize));
                    }
                }
            }
        }
        self.exec(&f.body);
        let result = self.read_bits(&Sig::wire(ret.wire, ret.width));
        self.scopes.pop();
        self.forget_locals(first_local);
        self.depth -= 1;
        if temp_proc {
            self.proc = None;
        }
        result
    }

    fn call_task(&mut self, t: &'a ast::SubroutineDecl, args: &[Expr], span: Span) {
        let ports: Vec<(&ast::PortDecl, &ast::Ident)> =
            t.ports.iter().flat_map(|p| p.names.iter().map(move |(n, _)| (p, n))).collect();
        if args.len() != ports.len() {
            self.error(
                span,
                format!(
                    "task `{}` takes {} arguments, {} given",
                    t.name.name,
                    ports.len(),
                    args.len()
                ),
            );
            return;
        }
        let mut inputs = Vec::new();
        for (i, (p, _)) in ports.iter().enumerate() {
            let (msb, lsb) = self.range(&p.range).unwrap_or((0, 0));
            let w = (msb - lsb).unsigned_abs() as u32 + 1;
            let v = if p.dir != ast::Direction::Output {
                let (aw, asg) = self.expr_type(&args[i]);
                let v = self.emit(&args[i], w.max(aw), asg);
                Some(Sig(v.0[..w as usize].to_vec()))
            } else {
                None
            };
            inputs.push((v, w, p.signed, msb, lsb));
        }
        let first_local = self.out.wires.len();
        let prefix = format!("{}.", self.fresh(&t.name.name));
        self.scopes.push(Scope { names: HashMap::new(), prefix });
        let mut locals = Vec::new();
        for ((_, n), (v, w, s, msb, lsb)) in ports.iter().zip(inputs) {
            let info = self.declare_local(n, w, s, msb, lsb);
            self.set_local(&info, &v.unwrap_or_else(|| Sig::xs(w as usize)));
            locals.push(info);
        }
        for d in &t.decls {
            if let ast::Item::Var(v) = d {
                self.local_vars(v);
            }
        }
        self.exec(&t.body);
        let outs: Vec<Sig> =
            locals.iter().map(|l| self.read_bits(&Sig::wire(l.wire, l.width))).collect();
        self.scopes.pop();
        self.forget_locals(first_local);
        for (i, (p, _)) in ports.iter().enumerate() {
            if p.dir != ast::Direction::Input
                && let Some(lv) = self.lvalue(&args[i], false)
            {
                let n = lv.width();
                let v = outs[i].extend(n, false);
                self.assign(lv, v, false, args[i].span());
            }
        }
    }

    /// Bitwise NOT of one bit (helper for callers outside `build`).
    #[allow(dead_code)]
    fn not_bit(&mut self, b: SigBit) -> SigBit {
        self.out.unary(UnaryOp::Not, Sig::bit(b)).0[0]
    }
}

/// Decodes a constant holding a Verilog string (8 bits per character, the
/// last character in the least significant byte; leading NULs are padding).
fn const_string(v: &LogicVec) -> Option<String> {
    let mut bytes = Vec::new();
    for i in (0..v.width().div_ceil(8)).rev() {
        let mut b = 0u8;
        for k in 0..8 {
            let idx = i * 8 + k;
            if idx < v.width() {
                match v.get(idx) {
                    Logic::One => b |= 1 << k,
                    Logic::Zero => {}
                    _ => return None,
                }
            }
        }
        if b != 0 || !bytes.is_empty() {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

impl Elab<'_> {
    fn hard_error(&mut self, span: Span, msg: impl Into<String>) {
        let d = Diagnostic::error(span, msg);
        match self.proc.as_mut() {
            Some(p) => p.hard_errors.push(d),
            None => self.diags.push(d),
        }
    }
}

/// Parses a `$readmemh`/`$readmemb` file into words with optional explicit
/// addresses (`@hex`).
pub fn read_mem_file(
    text: &str,
    hex: bool,
    width: usize,
) -> Result<Vec<(Option<u64>, LogicVec)>, String> {
    let mut out = Vec::new();
    let mut pending_addr = None;
    let mut cleaned = String::with_capacity(text.len());
    // Strip comments.
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'/') {
            for c2 in chars.by_ref() {
                if c2 == '\n' {
                    break;
                }
            }
            cleaned.push('\n');
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut prev = ' ';
            for c2 in chars.by_ref() {
                if prev == '*' && c2 == '/' {
                    break;
                }
                prev = c2;
            }
            cleaned.push(' ');
        } else {
            cleaned.push(c);
        }
    }
    for tok in cleaned.split_whitespace() {
        if let Some(a) = tok.strip_prefix('@') {
            pending_addr =
                Some(u64::from_str_radix(a, 16).map_err(|_| format!("bad address `{tok}`"))?);
            continue;
        }
        let v = LogicVec::parse_radix(tok, if hex { 16 } else { 2 }, width)
            .ok_or_else(|| format!("bad value `{tok}`"))?;
        out.push((pending_addr.take(), v));
    }
    Ok(out)
}
