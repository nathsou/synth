//! Declarations, statements, instances and function calls.

use std::collections::HashMap;

use synth_core::{BigInt, Diagnostic, LogicVec, Span};
use synth_rtl::{Attr, CellKind, Clock, Instance, MemWrite, Memory, PortDir, Sig, SigBit};

use super::expr::{adjust, const_bool, const_int, konst, one};
use super::*;
use crate::ast::{AssignOp, DomFieldValue, Expr, ExprKind, Stmt};

/// Bits of each signal assigned on every path through a statement.
pub(crate) type DefSet = HashMap<usize, Vec<bool>>;

fn mark(ds: &mut DefSet, sig: usize, width: u32, lo: u32, w: u32) {
    let v = ds.entry(sig).or_insert_with(|| vec![false; width as usize]);
    for b in &mut v[lo as usize..(lo + w) as usize] {
        *b = true;
    }
}

fn union(a: &mut DefSet, b: &DefSet) {
    for (k, v) in b {
        let e = a.entry(*k).or_insert_with(|| vec![false; v.len()]);
        for (x, y) in e.iter_mut().zip(v) {
            *x |= *y;
        }
    }
}

fn intersect(a: &DefSet, b: &DefSet) -> DefSet {
    let mut out = DefSet::new();
    for (k, v) in a {
        if let Some(w) = b.get(k) {
            out.insert(*k, v.iter().zip(w).map(|(x, y)| *x && *y).collect());
        }
    }
    out
}

/// An assignment target inside a signal.
pub(crate) struct Place {
    pub sig: usize,
    /// Alternative positions for runtime indices: (condition, low bit).
    pub alts: Vec<(SigBit, u32)>,
    pub width: u32,
    pub ty: Ty,
    pub tags: Tags,
}

pub(crate) enum Target {
    Sig(Place),
    Mem { mem: usize, addr: Hw, lo: u32, width: u32, ty: Ty, tags: Tags },
}

impl<'a> ModCx<'a> {
    pub fn resolve_lazy(&mut self, d: &mut Design<'a>, name: &str) -> R<Sym> {
        match self.lazy.get(name) {
            Some(Lazy::Done(s)) => return Ok(s.clone()),
            Some(Lazy::Failed) => return Err(()),
            Some(Lazy::Busy(sp)) => {
                let sp = *sp;
                let is_domain = self.body.iter().any(
                    |st| matches!(&st.kind, StmtKind::Domain { name: n, .. } if n.name == name),
                );
                let mut dg = Diagnostic::error(sp, format!("`{name}` depends on itself"))
                    .with_label("used in its own definition");
                dg = if is_domain {
                    dg.with_help(format!("the reset of domain `{name}` cannot come from a register in `{name}`: compute it in another domain on the same clock (for example `@clk`), or use `power_on_reset()`"))
                } else {
                    dg.with_help("break the cycle with a register, or declare a `wire` and drive it separately")
                };
                return d.err(dg);
            }
            Some(Lazy::Pending(_)) => {}
            None => unreachable!(),
        }
        let Some(Lazy::Pending(st)) =
            self.lazy.insert(name.to_string(), Lazy::Busy(Span::default()))
        else {
            unreachable!()
        };
        self.lazy.insert(name.to_string(), Lazy::Busy(decl_name_span(st)));
        let saved_scopes = std::mem::take(&mut self.scopes);
        let mut it = saved_scopes.into_iter();
        self.scopes = vec![it.next().unwrap_or_default()];
        let rest: Vec<Scope> = it.collect();
        let saved_loops = std::mem::take(&mut self.loop_names);
        let saved_rr = std::mem::replace(&mut self.reg_read, false);
        let r = self.declare(d, st, true);
        self.reg_read = saved_rr;
        self.loop_names = saved_loops;
        let base = self.scopes.pop().unwrap_or_default();
        self.scopes = std::iter::once(base).chain(rest).collect();
        match r {
            Ok(s) => {
                self.lazy.insert(name.to_string(), Lazy::Done(s.clone()));
                Ok(s)
            }
            Err(()) => {
                self.lazy.insert(name.to_string(), Lazy::Failed);
                Err(())
            }
        }
    }

    /// A `let` inside a block: evaluated in place.
    pub fn local_let(&mut self, d: &mut Design<'a>, st: &'a Stmt) -> R<()> {
        let StmtKind::Let { name, .. } = &st.kind else {
            return d.err(
                Diagnostic::error(st.span, "only `let` is allowed here").with_label("not a `let`"),
            );
        };
        self.check_fresh(d, name)?;
        let sym = self.declare(d, st, false)?;
        self.scopes.last_mut().unwrap().decls.insert(name.name.clone(), name.span);
        self.note_sym(d, name, &sym);
        self.scopes.last_mut().unwrap().syms.insert(name.name.clone(), sym);
        Ok(())
    }

    fn check_fresh(&mut self, d: &mut Design<'a>, name: &ast::Ident) -> R<()> {
        let visible = self.scopes.iter().any(|s| s.syms.contains_key(&name.name))
            || self.lazy.contains_key(&name.name)
            || self.generics.contains_key(&name.name);
        if visible {
            let prev = self.decl_span(&name.name);
            let mut dg =
                Diagnostic::error(name.span, format!("`{}` is already declared", name.name))
                    .with_label("shadows an existing name")
                    .with_help("Weft has no shadowing: pick a different name");
            if let Some(p) = prev
                && p != name.span
            {
                dg = dg.with_secondary(p, "first declared here");
            }
            return d.err(dg);
        }
        Ok(())
    }

    /// Elaborates a declaration and returns its symbol.
    pub fn declare(&mut self, d: &mut Design<'a>, st: &'a Stmt, module_level: bool) -> R<Sym> {
        let attrs = &st.attrs;
        match &st.kind {
            StmtKind::Const(c) => Ok(Sym::Value(self.const_decl_value(d, c)?)),
            StmtKind::Let { name, ty, dom, value } => {
                let ty = match ty {
                    Some(t) => Some(self.eval_type(d, t)?),
                    None => None,
                };
                let saved = (self.allow_module_call, self.call_name.take());
                self.allow_module_call = self.mode == Mode::Module;
                self.call_name = Some(self.name_of(&name.name));
                let v = self.eval(d, value, ty.as_ref());
                self.allow_module_call = saved.0;
                self.call_name = saved.1;
                let mut v = v?;
                if let Some(t) = &ty {
                    v = Value::Hw(self.coerce(d, v, t, value.span, &format!("`{}`", name.name), Some(value))?);
                }
                if let Value::Hw(h) = &mut v {
                    if let Some(r) = dom {
                        let dm = self.dom_ref(d, r)?;
                        if let Some(i) = dm {
                            let id = self.domains[i].clock.id;
                            self.sinks.push(Sink { clock: id, tags: h.tags.clone(), span: value.span, target: format!("`{}` (declared in domain `{}`)", name.name, self.domains[i].name) });
                            h.tags = Tags::one(TagAtom::Clock(id), name.span, format!("`{}` (in domain `{}`)", name.name, self.domains[i].name));
                        }
                    }
                    if self.mode == Mode::Module && !h.sig.is_const() {
                        let w = h.sig.width() as u32;
                        let wire = d.add_wire(&self.name_of(&name.name), w, name.span, true);
                        d.out.connect(wire.clone(), h.sig.clone());
                        h.sig = wire;
                    }
                }
                if self.mode == Mode::Module && matches!(v, Value::Hw(_)) && !name.name.starts_with('_') {
                    self.lets_read.entry(name.name.clone()).or_insert((name.span, false));
                }
                Ok(Sym::Value(v))
            }
            StmtKind::Var { .. } => d.err(
                Diagnostic::error(st.span, "`var` is only allowed in functions")
                    .with_label("mutable local")
                    .with_help("in a module, use `wire` for a signal driven by assignments, or `let` for a named value"),
            ),
            StmtKind::Wire { name, ty, dom } => {
                self.module_only(d, st, "wire")?;
                let t = self.eval_type(d, ty)?;
                let w = d.types.width(&t);
                let tag = match dom {
                    Some(r) => match self.dom_ref(d, r)? {
                        Some(i) => Some(self.dom_tag(i)),
                        None => Some(FixedTag { atom: Some(TagAtom::Async), what: "asynchronous".into() }),
                    },
                    None => None,
                };
                let sig = d.add_wire(&self.name_of(&name.name), w, name.span, true);
                self.apply_attrs(d, attrs, &sig);
                Ok(Sym::Signal(self.add_signal(name, SigKind::Wire, t, sig, tag)))
            }
            StmtKind::Reg { name, ty, dom, value, init_only } => {
                self.module_only(d, st, "reg")?;
                let t = self.eval_type(d, ty)?;
                let di = match dom {
                    Some(r) => self.dom_ref(d, r)?,
                    None => self.default_domain(d, name.span)?,
                };
                let Some(di) = di else {
                    if dom.is_some() {
                        return d.err(Diagnostic::error(name.span, "a register needs a clock, not `@async`").with_label("no clock"));
                    }
                    return d.err(self.no_default_error(d, name.span, &format!("register `{}`", name.name)));
                };
                if self.domains[di].clock.bit == SigBit::ZERO && self.is_top {
                    return d.err(
                        Diagnostic::error(name.span, format!("register `{}` needs a clock, but the top module has no clock input", name.name))
                            .with_label("no clock")
                            .with_help("add a clock input port: `in clk: clock`"),
                    );
                }
                let what = format!("the {} value of `{}`", if *init_only { "power-on" } else { "reset" }, name.name);
                let bits = self.const_bits(d, value, &t, &what)?;
                let w = d.types.width(&t);
                let sig = d.add_wire(&self.name_of(&name.name), w, name.span, true);
                self.apply_attrs(d, attrs, &sig);
                let tag = self.dom_tag(di);
                let kind = SigKind::Reg { dom: di, has_reset: !*init_only, value: bits };
                Ok(Sym::Signal(self.add_signal(name, kind, t, sig, Some(tag))))
            }
            StmtKind::Mem { name, elem, depth, dom, init } => {
                self.module_only(d, st, "mem")?;
                let et = self.eval_type(d, elem)?;
                let n = self.eval_int(d, depth)?;
                let Some(n) = n.to_u64().filter(|&n| (1..=1 << 24).contains(&n)) else {
                    return d.err(Diagnostic::error(depth.span, format!("memory depth must be between 1 and 16777216, not {n}")).with_label("bad depth"));
                };
                let di = match dom {
                    Some(r) => self.dom_ref(d, r)?,
                    None => self.default_domain(d, name.span)?,
                };
                let Some(di) = di else {
                    return d.err(self.no_default_error(d, name.span, &format!("memory `{}`", name.name)));
                };
                let ew = d.types.width(&et) as usize;
                let contents = match init {
                    None => vec![LogicVec::zeros(ew); n as usize],
                    Some(e) => self.mem_init(d, e, &et, n)?,
                };
                let id = d.out.memories.push(Memory {
                    name: self.name_of(&name.name),
                    width: ew as u32,
                    depth: n as u32,
                    offset: 0,
                    init: contents,
                    reads: vec![],
                    writes: vec![],
                    alive: true,
                });
                let idx = self.mems.len();
                self.mems.push(MemInfo {
                    name: name.name.clone(),
                    span: name.span,
                    elem: et,
                    depth: n,
                    dom: di,
                    id,
                    read: false,
                    written: false,
                    comb_reads: vec![],
                    has_init: init.is_some(),
                });
                Ok(Sym::Mem(idx))
            }
            StmtKind::Inst { name, call } => {
                self.module_only(d, st, "inst")?;
                let ExprKind::Call { callee, generics, args, dom } = &call.kind else { unreachable!() };
                let Some(m) = d.modules.get(callee.name.as_str()).copied() else {
                    let mut dg = Diagnostic::error(callee.span, format!("unknown module `{}`", callee.name)).with_label("not a module");
                    if d.fns.contains_key(&callee.name) {
                        dg = dg.with_help(format!("`{}` is a function: call it in an expression, `let x = {}(...);`", callee.name, callee.name));
                    } else if let Some(s) = suggest(&callee.name, d.modules.keys().map(|s| s.as_str())) {
                        dg = dg.with_help(format!("did you mean `{s}`?"));
                    } else {
                        dg = dg.with_help("define it, or declare a Verilog module with `extern module`");
                    }
                    return d.err(dg);
                };
                let iname = self.name_of(&name.name);
                let idx = self.instantiate(d, m, &iname, callee, generics, args, dom.as_ref(), call.span, false)?;
                self.insts[idx].name = name.name.clone();
                let _ = module_level;
                Ok(Sym::Inst(idx))
            }
            StmtKind::Domain { name, fields, .. } => {
                self.module_only(d, st, "domain")?;
                self.define_domain(d, name, fields)
            }
            _ => d.err(Diagnostic::error(st.span, "expected a declaration").with_label("not a declaration")),
        }
    }

    fn module_only(&mut self, d: &mut Design<'a>, st: &Stmt, kw: &str) -> R<()> {
        if self.mode == Mode::Function {
            return d.err(
                Diagnostic::error(st.span, format!("`{kw}` is not allowed in a function"))
                    .with_label("functions are pure logic")
                    .with_help("functions compute a value from their arguments; use a module for state and structure"),
            );
        }
        Ok(())
    }

    fn apply_attrs(&mut self, d: &mut Design<'a>, attrs: &[ast::Attribute], sig: &Sig) {
        let Some(SigBit::Wire(w, _)) = sig.0.first().copied() else { return };
        for a in attrs {
            match a.name.name.as_str() {
                "keep" => d.out.wires[w]
                    .attrs
                    .push(("keep".into(), Attr::Const(LogicVec::from_u64(1, 32)))),
                "async_reg" => {
                    d.out.wires[w].attrs.push(("async_reg".into(), Attr::Str("true".into())));
                    d.out.wires[w]
                        .attrs
                        .push(("keep".into(), Attr::Const(LogicVec::from_u64(1, 32))));
                }
                "attr" => {
                    let (Some(k), Some(v)) = (a.args.first(), a.args.get(1)) else {
                        d.emit(
                            Diagnostic::error(a.name.span, "`#[attr]` takes a name and a value")
                                .with_label("expected `#[attr(\"name\", value)]`"),
                        );
                        continue;
                    };
                    let ExprKind::Str(k) = &k.kind else {
                        d.emit(
                            Diagnostic::error(k.span, "the attribute name must be a string")
                                .with_label("not a string"),
                        );
                        continue;
                    };
                    let val = match &v.kind {
                        ExprKind::Str(s) => Attr::Str(s.clone()),
                        ExprKind::Int(n, _) => Attr::Const(n.to_logic_vec(32)),
                        _ => {
                            d.emit(
                                Diagnostic::error(
                                    v.span,
                                    "the attribute value must be a string or an integer",
                                )
                                .with_label("unsupported"),
                            );
                            continue;
                        }
                    };
                    d.out.wires[w].attrs.push((k.clone(), val));
                }
                other => {
                    let mut dg = Diagnostic::warning(
                        a.name.span,
                        format!("unknown attribute `{other}` is ignored"),
                    )
                    .with_label("unknown");
                    if let Some(s) = suggest(other, ["keep", "async_reg", "attr"]) {
                        dg = dg.with_help(format!("did you mean `#[{s}]`?"));
                    }
                    d.emit(dg);
                }
            }
        }
    }

    pub fn add_signal(
        &mut self,
        name: &ast::Ident,
        kind: SigKind,
        ty: Ty,
        sig: Sig,
        tag: Option<FixedTag>,
    ) -> usize {
        let i = self.signals.len();
        self.signals.push(Signal {
            name: name.name.clone(),
            span: name.span,
            kind,
            ty,
            sig,
            tag,
            read: false,
            assigned: false,
        });
        i
    }

    /// `@d`: a domain index, or `None` for `@async`.
    pub fn dom_ref(&mut self, d: &mut Design<'a>, r: &ast::DomRef) -> R<Option<usize>> {
        match &r.kind {
            ast::DomRefKind::Async => Ok(None),
            ast::DomRefKind::Default => match self.default_domain(d, r.span)? {
                Some(i) => Ok(Some(i)),
                None => d.err(self.no_default_error(d, r.span, "`@default`")),
            },
            ast::DomRefKind::Named(n) => match self.domain_by_name(d, &n.name, n.span)? {
                Some(i) => Ok(Some(i)),
                None => Err(()),
            },
        }
    }

    fn mem_init(&mut self, d: &mut Design<'a>, e: &'a Expr, et: &Ty, n: u64) -> R<Vec<LogicVec>> {
        let ew = d.types.width(et) as usize;
        if let ExprKind::Call { callee, args, .. } = &e.kind
            && (callee.name == "hex_file" || callee.name == "bin_file")
        {
            let Some(Arg { value: Expr { kind: ExprKind::Str(_), .. }, .. }) = args.first() else {
                return d.err(
                    Diagnostic::error(e.span, format!("`{}` takes a file name", callee.name))
                        .with_label("expected a string"),
                );
            };
            let path_v = self.eval(d, &args[0].value, None)?;
            let Value::Str(path) = path_v else { unreachable!() };
            let base = std::path::Path::new(&d.sm.file(e.span.file).name)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            let full = base.join(&path);
            let text = match std::fs::read_to_string(&full) {
                Ok(t) => t,
                Err(err) => {
                    return d.err(
                        Diagnostic::error(
                            args[0].value.span,
                            format!("cannot read `{}`: {err}", full.display()),
                        )
                        .with_label("memory contents file")
                        .with_help("paths are relative to the directory of this source file"),
                    );
                }
            };
            let words = match synth_elab::read_mem_file(&text, callee.name == "hex_file", ew) {
                Ok(w) => w,
                Err(err) => {
                    return d.err(
                        Diagnostic::error(
                            args[0].value.span,
                            format!("`{}`: {err}", full.display()),
                        )
                        .with_label("bad contents"),
                    );
                }
            };
            let mut out = vec![LogicVec::zeros(ew); n as usize];
            let mut addr = 0u64;
            for (a, v) in words {
                if let Some(a) = a {
                    addr = a;
                }
                if addr >= n {
                    return d.err(
                        Diagnostic::error(
                            args[0].value.span,
                            format!("`{}` has more than {n} words", full.display()),
                        )
                        .with_label("too many words for this memory"),
                    );
                }
                out[addr as usize] = v.resize(ew, false);
                addr += 1;
            }
            return Ok(out);
        }
        let t = Ty::Array(Box::new(et.clone()), n);
        let bits = self.const_bits(d, e, &t, "the initial contents")?;
        Ok((0..n as usize).map(|k| bits.slice((k * ew) as isize, ew)).collect())
    }

    fn define_domain(
        &mut self,
        d: &mut Design<'a>,
        name: &ast::Ident,
        fields: &'a [ast::DomField],
    ) -> R<Sym> {
        let (mut clock, mut reset, mut posedge) = (None, None, true);
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for f in fields {
            if let Some(prev) = seen.insert(&f.name.name, f.name.span) {
                d.emit(
                    Diagnostic::error(f.name.span, format!("`{}` is given twice", f.name.name))
                        .with_label("duplicate")
                        .with_secondary(prev, "first given here"),
                );
                continue;
            }
            match (f.name.name.as_str(), &f.value) {
                ("clock", DomFieldValue::Expr(e)) => {
                    let v = self.eval(d, e, None)?;
                    clock = Some(match v {
                        Value::Clock(c) => c,
                        Value::Domain(i) => self.domains[i].clock.clone(),
                        _ => {
                            return d.err(
                                Diagnostic::error(e.span, "`clock` needs a clock")
                                    .with_label("not a clock")
                                    .with_help("clocks come from clock input ports (`in clk: clock`), other domains (`sys.clock`) and instance clock outputs; clocks cannot be computed from logic, so use a clock enable instead"),
                            );
                        }
                    });
                }
                ("reset" | "async_reset", DomFieldValue::Expr(e)) => {
                    if reset.is_some() {
                        d.emit(
                            Diagnostic::error(f.name.span, "a domain has at most one reset")
                                .with_label("second reset"),
                        );
                        continue;
                    }
                    let h = self.eval_to(d, e, &Ty::Bool, "a reset")?;
                    reset = Some((h, f.name.name == "async_reset", e.span));
                }
                ("edge", DomFieldValue::Edge(r)) => posedge = *r,
                (other, _) => {
                    let mut dg =
                        Diagnostic::error(f.name.span, format!("unknown domain field `{other}`"))
                            .with_label("unknown");
                    if let Some(s) = suggest(other, ["clock", "reset", "async_reset", "edge"]) {
                        dg = dg.with_help(format!("did you mean `{s}`?"));
                    }
                    d.emit(dg);
                }
            }
        }
        let Some(clock) = clock else {
            return d.err(
                Diagnostic::error(name.span, format!("domain `{}` needs a clock", name.name))
                    .with_label("missing `clock: ...`")
                    .with_help(format!("write `domain {} {{ clock: clk }}`", name.name)),
            );
        };
        let idx = self.domains.len();
        let rs = match reset {
            Some((h, asynch, sp)) => {
                self.sinks.push(Sink {
                    clock: clock.id,
                    tags: h.tags.clone(),
                    span: sp,
                    target: format!("the reset of domain `{}`", name.name),
                });
                Some((h.sig.0[0], asynch))
            }
            None => None,
        };
        self.domains.push(DomainInfo { name: name.name.clone(), clock, posedge, reset: rs });
        Ok(Sym::Domain(idx))
    }

    /// Instantiates module `m` as `iname` (its IR name) and binds its ports.
    #[allow(clippy::too_many_arguments)]
    pub fn instantiate(
        &mut self,
        d: &mut Design<'a>,
        m: &'a ast::Module,
        iname: &str,
        callee: &'a ast::Ident,
        generics: &'a [ast::GenArg],
        args: &'a [Arg],
        dom: Option<&'a ast::DomRef>,
        span: Span,
        as_call: bool,
    ) -> R<usize> {
        if d.depth > 64 {
            return d.err(
                Diagnostic::error(
                    span,
                    format!("instantiating `{}` nests more than 64 levels deep", m.name.name),
                )
                .with_label("too deep")
                .with_help(
                    "a module that instantiates itself must stop through a constant condition",
                ),
            );
        }
        d.elaborated.insert(m.name.name.clone());
        if d.indexing {
            d.note(callee.span, Some(m.name.span), RefKind::Module, d.module_hover(m));
        }
        let mut child = ModCx::new(m, format!("{iname}."), false);
        child.bind_generic_list(
            d,
            &m.generics,
            &m.name.name,
            generics,
            HashMap::new(),
            span,
            Some(self),
        )?;
        let ports = child.port_types(d)?;
        // Arguments by port.
        let mut conn: HashMap<String, &'a Expr> = HashMap::new();
        let inputs: Vec<&PortInfo> = ports
            .iter()
            .filter(|p| {
                matches!(&p.kind, PortInfoKind::Data { dir: PortDir::Input, default: None, .. })
            })
            .collect();
        for a in args {
            let pname = match &a.name {
                Some(n) => n.clone(),
                None => {
                    if inputs.len() != 1 || args.len() != 1 {
                        return d.err(
                            Diagnostic::error(a.value.span, "module ports are connected by name")
                                .with_label("positional argument")
                                .with_help(format!("write `{}(port: value, ...)`; only a module with a single input may take it positionally", m.name.name)),
                        );
                    }
                    ast::Ident { name: inputs[0].name.clone(), span: a.value.span }
                }
            };
            if d.indexing
                && a.name.is_some()
                && let Some(port) = m.ports.iter().find(|p| p.name.name == pname.name)
            {
                d.note(pname.span, Some(port.name.span), RefKind::Port, d.port_hover(m, port));
            }
            let Some(p) = ports.iter().find(|p| p.name == pname.name) else {
                let mut dg = Diagnostic::error(
                    pname.span,
                    format!("`{}` has no port `{}`", m.name.name, pname.name),
                )
                .with_label("unknown port");
                if let Some(s) = suggest(&pname.name, ports.iter().map(|p| p.name.as_str())) {
                    dg = dg.with_help(format!("did you mean `{s}`?"));
                } else {
                    let ps: Vec<String> = ports.iter().map(|p| format!("`{}`", p.name)).collect();
                    dg = dg.with_help(format!("its ports are {}", ps.join(", ")));
                }
                dg = dg.with_secondary(m.name.span, format!("`{}` is defined here", m.name.name));
                d.emit(dg);
                continue;
            };
            if let PortInfoKind::Data { dir: PortDir::Output, .. } = p.kind {
                d.emit(
                    Diagnostic::error(
                        pname.span,
                        format!("`{}` is an output of `{}`", pname.name, m.name.name),
                    )
                    .with_label("outputs are not connected")
                    .with_help(format!(
                        "read it after the instantiation: `inst u = {}(...);` then `u.{}`",
                        m.name.name, pname.name
                    )),
                );
                continue;
            }
            if conn.insert(pname.name.clone(), &a.value).is_some() {
                d.emit(
                    Diagnostic::error(
                        pname.span,
                        format!("port `{}` is connected twice", pname.name),
                    )
                    .with_label("duplicate"),
                );
            }
        }
        // Domain parameters.
        let ndoms = ports
            .iter()
            .filter(|p| matches!(p.kind, PortInfoKind::Domain | PortInfoKind::Clock))
            .count()
            + usize::from(child.implicit_dom_needed());
        let fallback = |s: &mut Self, d: &mut Design<'a>, what: &str| -> R<usize> {
            if let Some(r) = dom {
                return match s.dom_ref(d, r)? {
                    Some(i) => Ok(i),
                    None => d.err(
                        Diagnostic::error(
                            r.span,
                            "an instance needs a clocked domain, not `@async`",
                        )
                        .with_label("no clock"),
                    ),
                };
            }
            match s.default_domain(d, span)? {
                Some(i) => Ok(i),
                None => d.err(
                    s.no_default_error(d, span, &format!("instance of `{}`", m.name.name))
                        .with_note(None, format!("it needs a domain for {what}")),
                ),
            }
        };
        if let Some(r) = dom
            && ndoms != 1
        {
            return d.err(
                Diagnostic::error(
                    r.span,
                    format!("`@` binds a module's only domain, but `{}` has {ndoms}", m.name.name),
                )
                .with_label("ambiguous")
                .with_help("connect each domain parameter by name: `fifo(wr: sys, rd: pix, ...)`"),
            );
        }
        let mut doms: HashMap<String, DomBinding> = HashMap::new();
        let mut parent_clock: HashMap<String, ClockId> = HashMap::new();
        let mut sigs = Vec::new();
        for p in &ports {
            match &p.kind {
                PortInfoKind::Domain | PortInfoKind::Clock => {
                    let pi = match conn.remove(&p.name) {
                        Some(e) => match self.eval(d, e, None)? {
                            Value::Domain(i) => Some((self.domains[i].clock.clone(), Some(i))),
                            Value::Clock(c) if matches!(p.kind, PortInfoKind::Clock) => {
                                Some((c, None))
                            }
                            _ => {
                                let what = if matches!(p.kind, PortInfoKind::Clock) {
                                    "a clock"
                                } else {
                                    "a domain"
                                };
                                return d.err(
                                    Diagnostic::error(
                                        e.span,
                                        format!("`{}` expects {what}", p.name),
                                    )
                                    .with_label(format!("not {what}")),
                                );
                            }
                        },
                        None if ndoms == 1 => {
                            let i = fallback(self, d, &format!("`{}`", p.name))?;
                            Some((self.domains[i].clock.clone(), Some(i)))
                        }
                        None => None,
                    };
                    let Some((clk, di)) = pi else {
                        return d.err(
                            Diagnostic::error(
                                span,
                                format!(
                                    "{} `{}` of `{}` is not connected",
                                    if matches!(p.kind, PortInfoKind::Clock) {
                                        "clock"
                                    } else {
                                        "domain"
                                    },
                                    p.name,
                                    m.name.name
                                ),
                            )
                            .with_label("missing connection")
                            .with_help(format!("connect it: `{}: <domain>`", p.name)),
                        );
                    };
                    parent_clock.insert(p.name.clone(), clk.id);
                    if matches!(p.kind, PortInfoKind::Clock) {
                        sigs.push(PortSig::Clock(clk.bit));
                    } else {
                        let (posedge, reset) = di
                            .map(|i| (self.domains[i].posedge, self.domains[i].reset))
                            .unwrap_or((true, None));
                        doms.insert(p.name.clone(), DomBinding { clk: clk.bit, posedge, reset });
                        sigs.push(PortSig::None);
                    }
                }
                PortInfoKind::Data { ty, .. } => {
                    let w = d.types.width(ty);
                    sigs.push(PortSig::Data(d.add_wire(
                        &format!("{iname}.{}", p.name),
                        w,
                        p.span,
                        true,
                    )));
                }
                PortInfoKind::ClockOut => {
                    sigs.push(PortSig::Clock(
                        d.add_wire(&format!("{iname}.{}", p.name), 1, p.span, true).0[0],
                    ));
                }
            }
        }
        if child.implicit_dom_needed() {
            let have = dom.is_some() || self.default_domain(d, span)?.is_some();
            if m.body.is_none() && !have {
                // An extern whose ports are all asynchronous needs no domain.
                doms.insert(
                    "default".into(),
                    DomBinding { clk: SigBit::ZERO, posedge: true, reset: None },
                );
            } else {
                let i = fallback(self, d, "its registers")?;
                let dm = &self.domains[i];
                parent_clock.insert("default".into(), dm.clock.id);
                doms.insert(
                    "default".into(),
                    DomBinding { clk: dm.clock.bit, posedge: dm.posedge, reset: dm.reset },
                );
            }
        }
        child.bind_ports(d, &ports, sigs.clone(), doms.clone())?;
        // Map the child's clocks to the parent's.
        let mut cmap: HashMap<ClockId, ClockId> = HashMap::new();
        for dm in &child.domains {
            let pid = match &dm.clock.origin {
                ClockOrigin::Port(n) | ClockOrigin::Param(n) => parent_clock.get(n).copied(),
                ClockOrigin::Internal => None,
            };
            let pid = pid.unwrap_or_else(|| d.fresh_clock(format!("{iname}.{}", dm.name)));
            cmap.insert(dm.clock.id, pid);
        }
        let idx = self.insts.len();
        let mut info = InstInfo {
            name: iname.to_string(),
            module: m.name.name.clone(),
            span: callee.span,
            ports: vec![],
            has_outputs: false,
            pending: vec![],
            clocks: vec![],
        };
        for (p, s) in ports.iter().zip(&sigs) {
            if let (PortInfoKind::ClockOut, PortSig::Clock(bit)) = (&p.kind, s) {
                let id = d.fresh_clock(format!("{iname}.{}", p.name));
                info.clocks.push((
                    p.name.clone(),
                    ClockVal { id, bit: *bit, origin: ClockOrigin::Internal },
                ));
                continue;
            }
            let (PortInfoKind::Data { dir, ty, default, .. }, PortSig::Data(sig)) = (&p.kind, s)
            else {
                continue;
            };
            let cs = child.signals.iter().find(|x| x.name == p.name).expect("port signal");
            let tag = match cs.tag.as_ref().and_then(|t| t.atom) {
                Some(TagAtom::Clock(c))
                    if cmap.get(&c).is_some_and(|pc| d.clockless.contains(pc)) =>
                {
                    FixedTag {
                        atom: if *dir == PortDir::Input { None } else { Some(TagAtom::Async) },
                        what: "asynchronous".into(),
                    }
                }
                Some(TagAtom::Clock(c)) => {
                    let pc = cmap
                        .get(&c)
                        .copied()
                        .unwrap_or_else(|| d.fresh_clock(format!("{iname}.?")));
                    let dname =
                        self.domains.iter().find(|x| x.clock.id == pc).map(|x| x.name.clone());
                    FixedTag {
                        atom: Some(TagAtom::Clock(pc)),
                        what: match dname {
                            Some(n) => format!("in domain `{n}`"),
                            None => format!("in a clock domain of `{}`", m.name.name),
                        },
                    }
                }
                _ => FixedTag {
                    atom: if *dir == PortDir::Input { None } else { Some(TagAtom::Async) },
                    what: "asynchronous".into(),
                },
            };
            let kind = if *dir == PortDir::Output {
                info.has_outputs = true;
                SigKind::InstOut { inst: idx }
            } else {
                SigKind::InstIn { inst: idx, default: default.clone() }
            };
            let pid = ast::Ident { name: format!("{}.{}", callee.name, p.name), span: p.span };
            let si = self.add_signal(&pid, kind, ty.clone(), sig.clone(), Some(tag));
            self.signals[si].name =
                format!("{}.{}", iname.rsplit('.').next().unwrap_or(iname), p.name);
            info.ports.push((p.name.clone(), si));
            if let Some(e) = conn.remove(&p.name) {
                info.pending.push((si, e));
            }
        }
        self.insts.push(info);
        // Elaborate the child.
        d.depth += 1;
        let r = if m.body.is_some() {
            child.run(d)
        } else {
            self.extern_instance(d, m, iname, &child, &ports, &sigs, &doms, span)
        };
        d.depth -= 1;
        r?;
        if as_call {
            let pend = std::mem::take(&mut self.insts[idx].pending);
            for (si, e) in pend {
                self.connect_input(d, si, e)?;
            }
        }
        Ok(idx)
    }

    /// Drives an instance input from its connection expression.
    pub fn connect_input(&mut self, d: &mut Design<'a>, si: usize, e: &'a Expr) -> R<()> {
        let ty = self.signals[si].ty.clone();
        let what = format!("input `{}`", self.signals[si].name);
        let h = self.eval_to(d, e, &ty, &what)?;
        let w = h.sig.width() as u32;
        self.drivers.push(Driver {
            sig: si,
            lo: 0,
            width: w,
            value: h.sig.clone(),
            guard: SigBit::ONE,
            span: e.span,
        });
        self.signals[si].assigned = true;
        self.connected.insert(si);
        let full = self.signals[si].sig.width() as u32;
        mark(&mut self.conn_defs, si, full, 0, full);
        self.flow(si, h.tags, e.span);
        Ok(())
    }

    /// Records that values with `tags` flow into signal `si`.
    pub fn flow(&mut self, si: usize, tags: Tags, span: Span) {
        let s = &self.signals[si];
        match &s.tag {
            None => self.var_sources.entry(si).or_default().join(&tags),
            Some(FixedTag { atom: Some(TagAtom::Clock(c)), what }) => {
                let target = match &s.kind {
                    SigKind::Reg { .. } => format!("register `{}` ({what})", s.name),
                    SigKind::Out => format!("output `{}` ({what})", s.name),
                    SigKind::InstIn { .. } => format!("input `{}` ({what})", s.name),
                    _ => format!("`{}` ({what})", s.name),
                };
                self.sinks.push(Sink { clock: *c, tags, span, target });
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn extern_instance(
        &mut self,
        d: &mut Design<'a>,
        m: &'a ast::Module,
        iname: &str,
        child: &ModCx<'a>,
        ports: &[PortInfo],
        sigs: &[PortSig],
        doms: &HashMap<String, DomBinding>,
        span: Span,
    ) -> R<()> {
        // Connections by Verilog port name.
        let mut conns: Vec<(String, PortDir, Sig)> = Vec::new();
        for (p, s) in ports.iter().zip(sigs) {
            match (&p.kind, s) {
                (PortInfoKind::Data { dir, .. }, PortSig::Data(sig)) => {
                    conns.push((p.name.clone(), *dir, sig.clone()))
                }
                (PortInfoKind::Clock, PortSig::Clock(b)) => {
                    conns.push((p.name.clone(), PortDir::Input, Sig::bit(*b)))
                }
                (PortInfoKind::ClockOut, PortSig::Clock(b)) => {
                    conns.push((p.name.clone(), PortDir::Output, Sig::bit(*b)))
                }
                _ => {}
            }
        }
        for port in &m.ports {
            if let PortKind::Binding { clock, reset } = &port.kind {
                let b = doms.get(&port.name.name).cloned().unwrap_or(DomBinding {
                    clk: SigBit::ZERO,
                    posedge: true,
                    reset: None,
                });
                conns.push((clock.name.clone(), PortDir::Input, Sig::bit(b.clk)));
                if let Some((r, low)) = reset {
                    let bit = b.reset.map(|(x, _)| x).unwrap_or(SigBit::ZERO);
                    let bit = if *low { d.out.not1(bit) } else { bit };
                    conns.push((r.name.clone(), PortDir::Input, Sig::bit(bit)));
                }
            }
        }
        let mut params = Vec::new();
        for g in &m.generics {
            match child.generics.get(&g.name.name) {
                Some(Value::Int(v, _)) => {
                    params.push((g.name.name.clone(), Attr::Const(v.to_logic_vec(32))))
                }
                Some(Value::Str(s)) => params.push((g.name.name.clone(), Attr::Str(s.clone()))),
                Some(Value::Hw(h)) if h.ty == Ty::Bool => params.push((
                    g.name.name.clone(),
                    Attr::Const(h.sig.as_const().unwrap().resize(1, false)),
                )),
                _ => {}
            }
        }
        if d.prim_files.contains(&m.name.span.file) {
            let cname = iname.to_string();
            d.out.add_cell(
                cname,
                CellKind::Instance(Box::new(Instance {
                    module: m.name.name.clone(),
                    params,
                    conns,
                })),
            );
            return Ok(());
        }
        super::inline::verilog_instance(d, m, iname, params, conns, span)
    }

    /// Walks the module body.
    pub fn walk_module(&mut self, d: &mut Design<'a>, body: &'a [Stmt]) -> R<()> {
        let r = self.walk(d, body, SigBit::ONE, &Tags::none(), false, true);
        let mut ds = r.clone().unwrap_or_default();
        let cd = std::mem::take(&mut self.conn_defs);
        union(&mut ds, &cd);
        self.conn_defs = cd;
        self.top_defs = ds;
        r.map(|_| ())
    }

    /// Walks statements under `guard`. `runtime` is set inside runtime
    /// branches, where declarations are not allowed.
    pub fn walk(
        &mut self,
        d: &mut Design<'a>,
        stmts: &'a [Stmt],
        guard: SigBit,
        gtags: &Tags,
        runtime: bool,
        top: bool,
    ) -> R<DefSet> {
        let mut defs: Vec<DefSet> = Vec::new();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut failed = false;
        for st in stmts {
            let start = self.drivers.len();
            match self.stmt(d, st, guard, gtags, runtime, top) {
                Ok(ds) => defs.push(ds),
                Err(()) => {
                    failed = true;
                    defs.push(DefSet::new());
                }
            }
            ranges.push((start, self.drivers.len()));
        }
        // Assignments that later statements always override.
        let mut suffix = DefSet::new();
        for i in (0..stmts.len()).rev() {
            let (s, e) = ranges[i];
            for k in s..e {
                if self.dead_warned.contains(&k) {
                    continue;
                }
                let dr = &self.drivers[k];
                let Some(bits) = suffix.get(&dr.sig) else { continue };
                if (dr.lo..dr.lo + dr.width).all(|b| bits[b as usize]) {
                    self.dead_warned.insert(k);
                    let name = self.signals[dr.sig].name.clone();
                    d.emit(
                        Diagnostic::warning(dr.span, format!("this assignment to `{name}` never takes effect"))
                            .with_label("always overridden by a later assignment")
                            .with_help("the last assignment that applies wins; remove this one, or make the later one conditional"),
                    );
                }
            }
            union(&mut suffix, &defs[i]);
        }
        if failed {
            return Err(());
        }
        let mut out = DefSet::new();
        for ds in &defs {
            union(&mut out, ds);
        }
        Ok(out)
    }

    fn stmt(
        &mut self,
        d: &mut Design<'a>,
        st: &'a Stmt,
        guard: SigBit,
        gtags: &Tags,
        runtime: bool,
        top: bool,
    ) -> R<DefSet> {
        match &st.kind {
            StmtKind::Const(_)
            | StmtKind::Wire { .. }
            | StmtKind::Reg { .. }
            | StmtKind::Mem { .. }
            | StmtKind::Inst { .. }
            | StmtKind::Domain { .. }
            | StmtKind::Let { .. } => {
                let name = match &st.kind {
                    StmtKind::Const(c) => &c.name,
                    StmtKind::Wire { name, .. }
                    | StmtKind::Reg { name, .. }
                    | StmtKind::Mem { name, .. }
                    | StmtKind::Inst { name, .. }
                    | StmtKind::Domain { name, .. }
                    | StmtKind::Let { name, .. } => name,
                    _ => unreachable!(),
                };
                let sym = if top {
                    if !self.lazy.contains_key(&name.name) {
                        return Ok(DefSet::new());
                    }
                    self.resolve_lazy(d, &name.name)?
                } else {
                    if runtime && !matches!(st.kind, StmtKind::Let { .. } | StmtKind::Const(_)) {
                        return d
                            .err(
                                Diagnostic::error(st.span, "declarations cannot depend on a runtime condition")
                                    .with_label("inside a runtime `if` or `match`")
                                    .with_help("hardware exists unconditionally: declare it at module level and use the condition to drive it"),
                            );
                    }
                    self.check_fresh(d, name)?;
                    let s = self.declare(d, st, false)?;
                    let sc = self.scopes.last_mut().unwrap();
                    sc.syms.insert(name.name.clone(), s.clone());
                    sc.decls.insert(name.name.clone(), name.span);
                    self.note_sym(d, name, &s);
                    s
                };
                if let Sym::Inst(i) = sym {
                    let pend = std::mem::take(&mut self.insts[i].pending);
                    for (si, e) in pend {
                        let _ = self.connect_input(d, si, e);
                    }
                }
                Ok(DefSet::new())
            }
            StmtKind::Var { .. } => d
                .err(
                    Diagnostic::error(st.span, "`var` is only allowed in functions")
                        .with_label("mutable local")
                        .with_help("in a module, use `wire` for a signal driven by assignments, or `let` for a named value"),
                ),
            StmtKind::Assign { target, op, value } => self.assign(d, st.span, target, *op, value, guard, gtags),
            StmtKind::If { cond, then, els, .. } => {
                let ch = self.eval_to(d, cond, &Ty::Bool, "the condition")?;
                let bit = ch.sig.0[0];
                if ch.tags.is_static() && bit.is_const() {
                    let branch: &'a [Stmt] = if bit == SigBit::ONE { then } else { els.as_deref().unwrap_or(&[]) };
                    self.scopes.push(Scope::default());
                    let r = self.walk(d, branch, guard, gtags, runtime, false);
                    self.scopes.pop();
                    return r;
                }
                let gt = gtags.clone().joined(&ch.tags);
                let (tn, en) = self.narrowing(d, cond);
                let g_then = d.out.and1(guard, bit);
                let nb = d.out.not1(bit);
                let g_else = d.out.and1(guard, nb);
                self.scopes.push(Scope { narrow: tn, ..Default::default() });
                let t = self.walk(d, then, g_then, &gt, true, false);
                self.scopes.pop();
                self.scopes.push(Scope { narrow: en, ..Default::default() });
                let e = match els {
                    Some(b) => self.walk(d, b, g_else, &gt, true, false),
                    None => Ok(DefSet::new()),
                };
                self.scopes.pop();
                let (t, e) = (t?, e?);
                let csnip = d.snippet(cond.span);
                for (si, bits) in &t {
                    let other = e.get(si);
                    if bits.iter().enumerate().any(|(k, b)| *b && !other.is_some_and(|o| o[k])) {
                        self.partial.push((*si, cond.span, format!("not assigned when `{csnip}` is false")));
                    }
                }
                for (si, bits) in &e {
                    let other = t.get(si);
                    if bits.iter().enumerate().any(|(k, b)| *b && !other.is_some_and(|o| o[k])) {
                        self.partial.push((*si, cond.span, format!("not assigned when `{csnip}` is true")));
                    }
                }
                Ok(intersect(&t, &e))
            }
            StmtKind::Match { scrutinee, arms } => {
                let sh = self.eval_hw(d, scrutinee, None)?;
                let pats: Vec<&ast::Pattern> = arms.iter().map(|(p, _)| p).collect();
                let infos = self.match_arms(d, &sh, scrutinee, &pats)?;
                let gt = gtags.clone().joined(&sh.tags);
                let name = match &scrutinee.strip().kind {
                    ExprKind::Ident(n) => Some(n.name.clone()),
                    _ => None,
                };
                if sh.tags.is_static() && sh.sig.is_const() {
                    for (k, info) in infos.iter().enumerate() {
                        if info.cond == SigBit::ONE {
                            self.scopes.push(Scope::default());
                            let r = self.walk(d, std::slice::from_ref(&arms[k].1), guard, gtags, runtime, false);
                            self.scopes.pop();
                            return r;
                        }
                    }
                    return Ok(DefSet::new());
                }
                let mut taken = SigBit::ZERO;
                let mut sets: Vec<DefSet> = Vec::new();
                let mut failed = false;
                for (k, info) in infos.iter().enumerate() {
                    let nt = d.out.not1(taken);
                    let eff = d.out.and1(info.cond, nt);
                    taken = d.out.or1(taken, info.cond);
                    let g = d.out.and1(guard, eff);
                    let mut sc = Scope::default();
                    if let (Some(n), Some(r)) = (&name, &info.narrow) {
                        sc.narrow.insert(n.clone(), r.clone());
                    }
                    self.scopes.push(sc);
                    let r = self.walk(d, std::slice::from_ref(&arms[k].1), g, &gt, true, false);
                    self.scopes.pop();
                    match r {
                        Ok(s) => sets.push(s),
                        Err(()) => {
                            failed = true;
                            sets.push(DefSet::new());
                        }
                    }
                }
                if failed {
                    return Err(());
                }
                let mut all = DefSet::new();
                for s in &sets {
                    union(&mut all, s);
                }
                for (si, bits) in &all {
                    for (k, s) in sets.iter().enumerate() {
                        let full = s.get(si).is_some_and(|b| b.iter().zip(bits).all(|(x, y)| *x || !*y));
                        if !full {
                            let p = d.snippet(arms[k].0.span);
                            self.partial.push((*si, arms[k].0.span, format!("not assigned in the arm `{p}`")));
                        }
                    }
                }
                let mut out = sets.first().cloned().unwrap_or_default();
                for s in sets.iter().skip(1) {
                    out = intersect(&out, s);
                }
                Ok(out)
            }
            StmtKind::For { var, start, end, inclusive, body } => {
                let a = self.eval_int(d, start)?;
                let mut b = self.eval_int(d, end)?;
                if *inclusive {
                    b = &b + &one();
                }
                let n = &b - &a;
                if n > BigInt::from_u64(1 << 16) {
                    return d
                        .err(Diagnostic::error(st.span, format!("this loop runs {n} times, more than the limit of 65536")).with_label("too many iterations"));
                }
                self.check_fresh(d, var)?;
                let mut out = DefSet::new();
                let mut i = a;
                while i < b {
                    let mut sc = Scope::default();
                    sc.syms.insert(var.name.clone(), Sym::Value(Value::Int(i.clone(), None)));
                    sc.decls.insert(var.name.clone(), var.span);
                    self.scopes.push(sc);
                    self.loop_names.push(format!("[{i}]"));
                    let r = self.walk(d, body, guard, gtags, runtime, false);
                    self.loop_names.pop();
                    self.scopes.pop();
                    union(&mut out, &r?);
                    i = &i + &one();
                }
                Ok(out)
            }
            StmtKind::Assert(a) => self.check_assert(d, a).map(|_| DefSet::new()),
            StmtKind::Block(b) => {
                self.scopes.push(Scope::default());
                let r = self.walk(d, b, guard, gtags, runtime, false);
                self.scopes.pop();
                r
            }
        }
    }

    /// Resolves an assignment target.
    fn target(&mut self, d: &mut Design<'a>, e: &'a Expr) -> R<Target> {
        match &e.kind {
            ExprKind::Paren(x) => self.target(d, x),
            ExprKind::Ident(id) => match self.lookup(d, id)? {
                Some(Sym::Signal(i)) => {
                    self.note_sym(d, id, &Sym::Signal(i));
                    let ty = self.signals[i].ty.clone();
                    let w = d.types.width(&ty);
                    Ok(Target::Sig(Place { sig: i, alts: vec![(SigBit::ONE, 0)], width: w, ty, tags: Tags::none() }))
                }
                Some(Sym::Mem(_)) => d
                    .err(
                        Diagnostic::error(e.span, format!("memory `{}` is written one element at a time", id.name))
                            .with_label("whole memory")
                            .with_help(format!("write `{}[address] := value;`", id.name)),
                    ),
                Some(Sym::Value(_)) => {
                    let is_let = self.let_names.contains(&id.name) || self.scopes.iter().any(|s| s.syms.contains_key(&id.name));
                    let mut dg = Diagnostic::error(e.span, format!("`{}` cannot be assigned", id.name));
                    if is_let {
                        dg = dg
                            .with_label("a `let` value")
                            .with_help(format!("a `let` is fixed by its definition; to drive a signal conditionally, declare `wire {}: <type>;` and assign it", id.name));
                    } else {
                        dg = dg.with_label("a constant");
                    }
                    if let Some(sp) = self.decl_span(&id.name) {
                        dg = dg.with_secondary(sp, "declared here");
                    }
                    d.err(dg)
                }
                Some(Sym::Domain(_) | Sym::ClockPort(_)) => d
                    .err(Diagnostic::error(e.span, format!("`{}` is a domain and cannot be assigned", id.name)).with_label("domain")),
                Some(Sym::Inst(_)) => d
                    .err(
                        Diagnostic::error(e.span, format!("`{}` is an instance; assign one of its inputs", id.name))
                            .with_label("instance")
                            .with_help(format!("write `{}.port = value;`", id.name)),
                    ),
                Some(Sym::Var(_)) => unreachable!(),
                None => {
                    self.eval_ident(d, id)?;
                    Err(())
                }
            },
            ExprKind::Field(base, name) => {
                if let ExprKind::Ident(id) = &base.strip().kind
                    && let Some(Sym::Inst(i)) = self.lookup(d, id)?
                {
                    let inst = self.insts[i].clone();
                    let Some(&(_, si)) = inst.ports.iter().find(|(n, _)| *n == name.name) else {
                        let mut dg = Diagnostic::error(name.span, format!("instance `{}` has no port `{}`", id.name, name.name)).with_label("unknown port");
                        if let Some(s) = suggest(&name.name, inst.ports.iter().map(|(n, _)| n.as_str())) {
                            dg = dg.with_help(format!("did you mean `{}.{s}`?", id.name));
                        }
                        return d.err(dg);
                    };
                    if d.indexing {
                        self.note_sym(d, id, &Sym::Inst(i));
                        let decl = d.port_of(&inst.module, &name.name).map(|p| p.name.span);
                        let hover = self.describe_signal(d, si) + &format!("\n\nport of instance `{}` of `{}`", inst.name, inst.module);
                        d.note(name.span, decl, RefKind::Port, hover);
                    }
                    let ty = self.signals[si].ty.clone();
                    let w = d.types.width(&ty);
                    return Ok(Target::Sig(Place { sig: si, alts: vec![(SigBit::ONE, 0)], width: w, ty, tags: Tags::none() }));
                }
                let t = self.target(d, base)?;
                let Target::Sig(mut p) = t else {
                    return self.mem_sub_target(d, t, e, |s, d, ty| match ty {
                        Ty::Struct(si) => s.field_layout(d, *si, &name.name),
                        _ => None,
                    });
                };
                let Ty::Struct(sti) = p.ty else {
                    let tn = d.types.name(&p.ty);
                    return d.err(Diagnostic::error(name.span, format!("{tn} has no field `{}`", name.name)).with_label("no such field"));
                };
                let Some((off, w, ty)) = self.field_layout(d, sti, &name.name) else {
                    let info = &d.types.structs[sti];
                    let mut dg = Diagnostic::error(name.span, format!("`{}` has no field `{}`", info.name, name.name)).with_label("unknown field");
                    if let Some(s) = suggest(&name.name, info.fields.iter().map(|(n, _)| n.as_str())) {
                        dg = dg.with_help(format!("did you mean `.{s}`?"));
                    }
                    return d.err(dg);
                };
                for a in &mut p.alts {
                    a.1 += off;
                }
                p.width = w;
                p.ty = ty;
                Ok(Target::Sig(p))
            }
            ExprKind::Index(base, idx) => {
                // Memory element.
                if let ExprKind::Ident(id) = &base.strip().kind
                    && let Some(Sym::Mem(mi)) = self.lookup(d, id)?
                {
                    let ah = self.eval_hw(d, idx, None)?;
                    let depth = self.mems[mi].depth;
                    self.check_index_pub(d, &ah, idx.span, depth, &format!("memory `{}`", id.name))?;
                    let ty = self.mems[mi].elem.clone();
                    let w = d.types.width(&ty);
                    let tags = ah.tags.clone();
                    return Ok(Target::Mem { mem: mi, addr: ah, lo: 0, width: w, ty, tags });
                }
                let t = self.target(d, base)?;
                let Target::Sig(mut p) = t else {
                    // Bits of a memory word.
                    let k = self.eval_int(d, idx)?;
                    let Some(k) = k.to_u64() else { return Err(()) };
                    return self.mem_sub_target(d, t, e, move |_, _, ty| match ty {
                        Ty::Int(i) if k < i.width() as u64 => Some((k as u32, 1, Ty::Bool)),
                        _ => None,
                    });
                };
                let (ew, n, ety) = match &p.ty {
                    Ty::Array(et, n) => (d.types.width(et), *n, (**et).clone()),
                    Ty::Int(i) => (1, i.width() as u64, Ty::Bool),
                    t => {
                        let tn = d.types.name(t);
                        return d.err(Diagnostic::error(base.span, format!("cannot index {tn}")).with_label(tn));
                    }
                };
                let iv = self.eval(d, idx, None)?;
                match iv {
                    Value::Int(k, _) => {
                        let Some(k) = k.to_u64().filter(|&k| k < n) else {
                            return d
                                .err(Diagnostic::error(idx.span, format!("index {k} is out of range: there are {n} elements")).with_label(format!("valid indices are 0..={}", n - 1)));
                        };
                        for a in &mut p.alts {
                            a.1 += k as u32 * ew;
                        }
                    }
                    other => {
                        let ih = self.to_hw(d, other, idx.span)?;
                        self.check_index_pub(d, &ih, idx.span, n, &format!("`{}`", d.snippet(base.span)))?;
                        let Ty::Int(it) = &ih.ty else { unreachable!() };
                        let (lo, hi) = (it.lo.to_u64().unwrap_or(0), it.hi.to_u64().unwrap_or(n - 1).min(n - 1));
                        let mut alts = Vec::new();
                        for (c, base_lo) in &p.alts {
                            for k in lo..=hi {
                                let kc = BigInt::from_u64(k).to_logic_vec(ih.sig.width());
                                let eq = d.out.eq_const(ih.sig.clone(), &kc);
                                let cond = d.out.and1(*c, eq);
                                alts.push((cond, base_lo + k as u32 * ew));
                            }
                        }
                        p.alts = alts;
                        p.tags.join(&ih.tags);
                    }
                }
                p.width = ew;
                p.ty = ety;
                Ok(Target::Sig(p))
            }
            ExprKind::Slice(base, hi, lo) => {
                let h = self.eval_int(d, hi)?;
                let l = self.eval_int(d, lo)?;
                let t = self.target(d, base)?;
                let (Some(h), Some(l)) = (h.to_u64(), l.to_u64()) else { return Err(()) };
                if l > h {
                    return d.err(Diagnostic::error(e.span, "slices are written high to low").with_label("reversed slice"));
                }
                let n = (h - l + 1) as u32;
                let ty = Ty::Int(IntTy::decl(IntDecl::Uint(n)));
                match t {
                    Target::Sig(mut p) => {
                        let Ty::Int(i) = &p.ty else {
                            let tn = d.types.name(&p.ty);
                            return d.err(Diagnostic::error(base.span, format!("cannot slice {tn}")).with_label(tn));
                        };
                        if h >= i.width() as u64 {
                            return d
                                .err(Diagnostic::error(e.span, format!("bit {h} is out of range: the target has {} bits", i.width())).with_label("out of range"));
                        }
                        for a in &mut p.alts {
                            a.1 += l as u32;
                        }
                        p.width = n;
                        p.ty = ty;
                        Ok(Target::Sig(p))
                    }
                    t => self.mem_sub_target(d, t, e, move |_, _, mty| match mty {
                        Ty::Int(i) if h < i.width() as u64 => Some((l as u32, n, Ty::Int(IntTy::decl(IntDecl::Uint(n))))),
                        _ => None,
                    }),
                }
            }
            _ => d
                .err(
                    Diagnostic::error(e.span, "cannot assign to this expression")
                        .with_label("not a signal")
                        .with_help("the left side of `=` or `:=` is a signal, optionally with `.field`, `[index]` or `[hi:lo]`"),
                ),
        }
    }

    fn mem_sub_target(
        &mut self,
        d: &mut Design<'a>,
        t: Target,
        e: &'a Expr,
        f: impl FnOnce(&Self, &Design<'a>, &Ty) -> Option<(u32, u32, Ty)>,
    ) -> R<Target> {
        let Target::Mem { mem, addr, lo, ty, tags, .. } = t else { unreachable!() };
        match f(self, d, &ty) {
            Some((off, w, nty)) => {
                Ok(Target::Mem { mem, addr, lo: lo + off, width: w, ty: nty, tags })
            }
            None => d.err(
                Diagnostic::error(e.span, "invalid part of a memory word")
                    .with_label("cannot select this"),
            ),
        }
    }

    pub fn check_index_pub(
        &mut self,
        d: &mut Design<'a>,
        idx: &Hw,
        span: Span,
        len: u64,
        what: &str,
    ) -> R<()> {
        let Ty::Int(i) = &idx.ty else {
            return d.err(
                Diagnostic::error(span, "an index must be an integer").with_label("not an integer"),
            );
        };
        let max = BigInt::from_u64(len - 1);
        if i.lo.is_negative() || i.hi > max {
            let snip = d.snippet(span);
            return d.err(
                Diagnostic::error(span, format!("index `{snip}` may be out of range: {what} has {len} entries"))
                    .with_label(format!("values {}, but valid indices are 0..={max}", i.range_text()))
                    .with_help(format!("use an `index<{len}>` value, or guard the access: `if {snip} < {len} {{ ... }}`")),
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn assign(
        &mut self,
        d: &mut Design<'a>,
        span: Span,
        target: &'a Expr,
        op: AssignOp,
        value: &'a Expr,
        guard: SigBit,
        gtags: &Tags,
    ) -> R<DefSet> {
        let t = self.target(d, target)?;
        let tsnip = d.snippet(target.span);
        match t {
            Target::Mem { mem, addr, lo, width, ty, tags } => {
                if op == AssignOp::Drive {
                    return d.err(
                        Diagnostic::error(
                            span,
                            format!("memory `{}` is written with `:=`", self.mems[mem].name),
                        )
                        .with_label("`=` drives combinational signals")
                        .with_help(format!(
                            "write `{tsnip} := ...;`: the write happens at the next clock edge"
                        )),
                    );
                }
                let h = self.eval_to(d, value, &ty, &format!("`{tsnip}`"))?;
                let info = &self.mems[mem];
                let dm = &self.domains[info.dom];
                let ew = d.types.width(&info.elem) as usize;
                let aw = bits_unsigned(&BigInt::from_u64(info.depth - 1)) as usize;
                let mut en = Sig::zeros(ew);
                let mut data = Sig::zeros(ew);
                for k in 0..width as usize {
                    en.0[lo as usize + k] = guard;
                    data.0[lo as usize + k] = h.sig.0[k];
                }
                let id = info.id;
                let clk = Clock { sig: dm.clock.bit, posedge: dm.posedge };
                let clock_id = dm.clock.id;
                let dname = dm.name.clone();
                let mname = info.name.clone();
                d.out.memories[id].writes.push(MemWrite {
                    clk,
                    en,
                    addr: addr.sig.extend(aw, false).slice(0, aw),
                    data,
                });
                self.mems[mem].written = true;
                let all = h.tags.joined(gtags).joined(&tags);
                self.sinks.push(Sink {
                    clock: clock_id,
                    tags: all,
                    span: value.span,
                    target: format!("memory `{mname}` (in domain `{dname}`)"),
                });
                Ok(DefSet::new())
            }
            Target::Sig(p) => {
                let s = &self.signals[p.sig];
                let name = s.name.clone();
                let decl = s.span;
                match (&s.kind, op) {
                    (SigKind::Reg { .. }, AssignOp::Drive) => {
                        return d.err(
                            Diagnostic::error(
                                span,
                                format!("`{name}` is a register: set its next value with `:=`"),
                            )
                            .with_label("`=` drives combinational signals")
                            .with_secondary(decl, "declared as a register here")
                            .with_help(format!("write `{tsnip} := ...;`")),
                        );
                    }
                    (SigKind::Wire | SigKind::Out | SigKind::InstIn { .. }, AssignOp::Next) => {
                        return d
                            .err(
                                Diagnostic::error(span, format!("`{name}` is combinational: assign it with `=`"))
                                    .with_label("`:=` sets a register's next value")
                                    .with_secondary(decl, "declared here")
                                    .with_help(format!("write `{tsnip} = ...;`, or declare it with `reg` to make it a register")),
                            );
                    }
                    (SigKind::In, _) => {
                        return d.err(
                            Diagnostic::error(
                                target.span,
                                format!("`{name}` is an input and cannot be assigned"),
                            )
                            .with_label("input port")
                            .with_secondary(decl, "declared here"),
                        );
                    }
                    (SigKind::InstOut { inst }, _) => {
                        let iname = self.insts[*inst].name.clone();
                        return d.err(
                            Diagnostic::error(
                                target.span,
                                format!("`{name}` is an output of instance `{iname}`"),
                            )
                            .with_label("driven by the instance")
                            .with_help("instance outputs are read, not assigned"),
                        );
                    }
                    _ => {}
                }
                if self.connected.contains(&p.sig) {
                    let iname = match &self.signals[p.sig].kind {
                        SigKind::InstIn { inst, .. } => self.insts[*inst].name.clone(),
                        _ => String::new(),
                    };
                    return d.err(
                        Diagnostic::error(
                            span,
                            format!(
                                "`{name}` is already connected where `{iname}` is instantiated"
                            ),
                        )
                        .with_label("second driver")
                        .with_help(
                            "connect it either in the instantiation or with assignments, not both",
                        ),
                    );
                }
                let is_reg = matches!(self.signals[p.sig].kind, SigKind::Reg { .. });
                self.reg_read = is_reg;
                let what = match &self.signals[p.sig].kind {
                    SigKind::Reg { .. } => format!("register `{tsnip}`"),
                    SigKind::Out => format!("output `{tsnip}`"),
                    SigKind::InstIn { .. } => format!("input `{tsnip}`"),
                    _ => format!("`{tsnip}`"),
                };
                let h = self.eval_to(d, value, &p.ty, &what);
                self.reg_read = false;
                let h = h?;
                // `r := r;`
                if is_reg
                    && let (ExprKind::Ident(a), ExprKind::Ident(b)) =
                        (&target.strip().kind, &value.strip().kind)
                    && a.name == b.name
                {
                    d.emit(
                        Diagnostic::warning(span, format!("`{name} := {name};` has no effect"))
                            .with_label(
                                "a register already keeps its value when it is not assigned",
                            )
                            .with_help("remove this assignment"),
                    );
                }
                for (c, lo) in &p.alts {
                    let g = d.out.and1(guard, *c);
                    self.drivers.push(Driver {
                        sig: p.sig,
                        lo: *lo,
                        width: p.width,
                        value: h.sig.clone(),
                        guard: g,
                        span,
                    });
                }
                self.signals[p.sig].assigned = true;
                let tags = h.tags.joined(gtags).joined(&p.tags);
                self.flow(p.sig, tags, value.span);
                let mut ds = DefSet::new();
                if p.alts.len() == 1 && p.alts[0].0 == SigBit::ONE {
                    let w = self.signals[p.sig].sig.width() as u32;
                    mark(&mut ds, p.sig, w, p.alts[0].1, p.width);
                }
                Ok(ds)
            }
        }
    }

    /// Calls a function: arguments are evaluated here, the body is inlined.
    pub fn call_fn(
        &mut self,
        d: &mut Design<'a>,
        f: &'a ast::Function,
        generics: &'a [ast::GenArg],
        args: &'a [Arg],
        span: Span,
    ) -> R<Value> {
        if d.fn_depth >= 256 {
            return d.err(
                Diagnostic::error(
                    span,
                    format!("calls to `{}` nest more than 256 deep", f.name.name),
                )
                .with_label("too deep")
                .with_help("recursion must end through a constant condition"),
            );
        }
        if args.len() != f.params.len() {
            return d.err(
                Diagnostic::error(
                    span,
                    format!(
                        "`{}` takes {} argument{}, but {} were given",
                        f.name.name,
                        f.params.len(),
                        if f.params.len() == 1 { "" } else { "s" },
                        args.len()
                    ),
                )
                .with_label("wrong number of arguments")
                .with_secondary(f.name.span, "defined here"),
            );
        }
        if let Some(n) = args.iter().find_map(|a| a.name.as_ref()) {
            return d.err(
                Diagnostic::error(n.span, "function arguments are positional")
                    .with_label("named argument")
                    .with_help("remove `name:`"),
            );
        }
        let mut frame = ModCx::global();
        frame.def_name = f.name.name.clone();
        frame.file = f.name.span.file;
        frame.generic_decls =
            f.generics.iter().map(|g| (g.name.name.clone(), g.name.span)).collect();
        // Explicit generic arguments.
        let mut given: HashMap<String, Value> = HashMap::new();
        for a in generics {
            let Some(g) = super::generic_param(d, &f.generics, &f.name.name, a) else {
                return Err(());
            };
            let v = self.eval_generic_arg(d, g, &a.value)?;
            given.insert(g.name.name.clone(), v);
        }
        frame.generics = given;
        let unbound: Vec<String> = f
            .generics
            .iter()
            .map(|g| g.name.name.clone())
            .filter(|n| !frame.generics.contains_key(n))
            .collect();
        // Arguments: typed ones first get their expected type.
        let mut vals: Vec<Option<Value>> = vec![None; args.len()];
        for (k, (_, pty)) in f.params.iter().enumerate() {
            if !mentions(pty, &unbound) {
                let t = frame.eval_type(d, pty)?;
                vals[k] = Some(self.eval(d, &args[k].value, Some(&t))?);
            } else {
                let v = self.eval(d, &args[k].value, None)?;
                if let Value::Hw(h) = &v {
                    infer(d, &mut frame, pty, &h.ty);
                }
                vals[k] = Some(v);
            }
        }
        for g in &f.generics {
            if frame.generics.contains_key(&g.name.name) {
                continue;
            }
            match &g.default {
                Some(def) => {
                    let v = frame.eval_generic_arg(d, g, def)?;
                    frame.generics.insert(g.name.name.clone(), v);
                }
                None => {
                    return d.err(
                        Diagnostic::error(
                            span,
                            format!(
                                "cannot infer `{}` for this call to `{}`",
                                g.name.name, f.name.name
                            ),
                        )
                        .with_label("unknown generic argument")
                        .with_help(format!(
                            "pass it explicitly: `{}<{}: ...>(...)`",
                            f.name.name, g.name.name
                        )),
                    );
                }
            }
        }
        for (k, (pname, pty)) in f.params.iter().enumerate() {
            let t = frame.eval_type(d, pty)?;
            let v = vals[k].take().unwrap();
            let h = self.coerce(
                d,
                v,
                &t,
                args[k].value.span,
                &format!("parameter `{}` of `{}`", pname.name, f.name.name),
                Some(&args[k].value),
            )?;
            frame.scopes[0].syms.insert(pname.name.clone(), Sym::Value(Value::Hw(h)));
            frame.scopes[0].decls.insert(pname.name.clone(), pname.span);
            if d.indexing {
                let sym = frame.scopes[0].syms[&pname.name].clone();
                frame.note_sym(d, pname, &sym);
            }
        }
        let rt = frame.eval_type(d, &f.ret)?;
        d.fn_depth += 1;
        let r = (|| {
            frame.exec(d, &f.body)?;
            let v = frame.eval(d, &f.result, Some(&rt))?;
            frame.coerce(
                d,
                v,
                &rt,
                f.result.span,
                &format!("the result of `{}`", f.name.name),
                Some(&f.result),
            )
        })();
        d.fn_depth -= 1;
        let mut h = r?;
        // The result carries the join of the arguments' tags (already in h).
        if !h.tags.is_static() {
            h.tags.0.truncate(h.tags.0.len());
        }
        let _ = const_bool;
        Ok(Value::Hw(h))
    }

    /// Executes function statements sequentially.
    fn exec(&mut self, d: &mut Design<'a>, stmts: &'a [Stmt]) -> R<()> {
        for st in stmts {
            self.exec_stmt(d, st)?;
        }
        Ok(())
    }

    fn vars(&self) -> Vec<(usize, String, Hw)> {
        let mut out = Vec::new();
        for (k, s) in self.scopes.iter().enumerate() {
            for (n, sym) in &s.syms {
                if let Sym::Var(h) = sym {
                    out.push((k, n.clone(), h.clone()));
                }
            }
        }
        out
    }

    fn set_var(&mut self, scope: usize, name: &str, h: Hw) {
        self.scopes[scope].syms.insert(name.to_string(), Sym::Var(h));
    }

    /// Runs `stmts` in a new scope and returns the values of the variables
    /// `before` (from [`Self::vars`] before the branching statement) at its
    /// end, in the same order, restoring their values from `before`.
    /// Variables are looked up by name: [`Self::vars`] lists them in hash
    /// map order, which can differ after a branch has run.
    fn exec_branch(
        &mut self,
        d: &mut Design<'a>,
        stmts: &'a [Stmt],
        narrow: HashMap<String, IntTy>,
        before: &[(usize, String, Hw)],
    ) -> R<Vec<Hw>> {
        self.scopes.push(Scope { narrow, ..Default::default() });
        let r = self.exec(d, stmts);
        self.scopes.pop();
        r?;
        let after: Vec<Hw> = before
            .iter()
            .map(|(k, n, _)| match &self.scopes[*k].syms[n] {
                Sym::Var(h) => h.clone(),
                _ => unreachable!(),
            })
            .collect();
        for (k, n, h) in before {
            self.set_var(*k, n, h.clone());
        }
        Ok(after)
    }

    fn exec_stmt(&mut self, d: &mut Design<'a>, st: &'a Stmt) -> R<()> {
        match &st.kind {
            StmtKind::Let { .. } => self.local_let(d, st),
            StmtKind::Const(c) => {
                self.check_fresh(d, &c.name)?;
                let v = self.const_decl_value(d, c)?;
                let sc = self.scopes.last_mut().unwrap();
                sc.syms.insert(c.name.name.clone(), Sym::Value(v));
                sc.decls.insert(c.name.name.clone(), c.name.span);
                Ok(())
            }
            StmtKind::Var { name, ty, value } => {
                self.check_fresh(d, name)?;
                let h = match ty {
                    Some(t) => {
                        let t = self.eval_type(d, t)?;
                        self.eval_to(d, value, &t, &format!("`{}`", name.name))?
                    }
                    None => {
                        let v = self.eval(d, value, None)?;
                        if let Value::Int(c, _) = &v {
                            return d.err(
                                Diagnostic::error(
                                    name.span,
                                    format!("`var {}` needs a type", name.name),
                                )
                                .with_label("the type is not known from a constant")
                                .with_help(format!("write `var {}: uint<8> = {c};`", name.name)),
                            );
                        }
                        let mut h = self.to_hw(d, v, value.span)?;
                        // A var keeps its declared range: widen a range type to its natural declared type.
                        if let Ty::Int(i) = &h.ty
                            && i.full_decl().is_none()
                        {
                            let t = Ty::Int(IntTy::decl(i.natural_decl()));
                            h =
                                self.coerce(d, Value::Hw(h), &t, value.span, "the variable", None)?;
                        }
                        h
                    }
                };
                let sc = self.scopes.last_mut().unwrap();
                sc.syms.insert(name.name.clone(), Sym::Var(h));
                sc.decls.insert(name.name.clone(), name.span);
                Ok(())
            }
            StmtKind::Assign { target, op, value } => {
                if *op == AssignOp::Next {
                    return d.err(
                        Diagnostic::error(st.span, "functions have no registers: use `=`")
                            .with_label("`:=` sets a register's next value")
                            .with_help("assign a `var` with `=`"),
                    );
                }
                self.assign_var(d, target, value)
            }
            StmtKind::If { cond, then, els, .. } => {
                let ch = self.eval_to(d, cond, &Ty::Bool, "the condition")?;
                let bit = ch.sig.0[0];
                if ch.tags.is_static() && bit.is_const() {
                    let b: &'a [Stmt] =
                        if bit == SigBit::ONE { then } else { els.as_deref().unwrap_or(&[]) };
                    self.scopes.push(Scope::default());
                    let r = self.exec(d, b);
                    self.scopes.pop();
                    return r;
                }
                let (tn, en) = self.narrowing(d, cond);
                let before = self.vars();
                let tv = self.exec_branch(d, then, tn, &before)?;
                let ev = match els {
                    Some(b) => self.exec_branch(d, b, en, &before)?,
                    None => before.iter().map(|(_, _, h)| h.clone()).collect(),
                };
                for ((k, n, _), (t, e)) in before.iter().zip(tv.into_iter().zip(ev)) {
                    let sig = d.out.mux(bit, e.sig.clone(), t.sig.clone());
                    let tags = t.tags.joined(&e.tags).joined(&ch.tags);
                    self.set_var(*k, n, Hw { sig, ty: t.ty, tags });
                }
                Ok(())
            }
            StmtKind::Match { scrutinee, arms } => {
                let sh = self.eval_hw(d, scrutinee, None)?;
                let pats: Vec<&ast::Pattern> = arms.iter().map(|(p, _)| p).collect();
                let infos = self.match_arms(d, &sh, scrutinee, &pats)?;
                let name = match &scrutinee.strip().kind {
                    ExprKind::Ident(n) => Some(n.name.clone()),
                    _ => None,
                };
                let before = self.vars();
                let mut results = Vec::new();
                for (k, info) in infos.iter().enumerate() {
                    let mut nar = HashMap::new();
                    if let (Some(n), Some(r)) = (&name, &info.narrow) {
                        nar.insert(n.clone(), r.clone());
                    }
                    results.push(self.exec_branch(
                        d,
                        std::slice::from_ref(&arms[k].1),
                        nar,
                        &before,
                    )?);
                }
                // Priority chain, last arm first.
                for (vi, (k, n, _)) in before.iter().enumerate() {
                    let mut cur = results
                        .last()
                        .map(|r| r[vi].clone())
                        .unwrap_or_else(|| before[vi].2.clone());
                    for a in (0..infos.len().saturating_sub(1)).rev() {
                        let t = &results[a][vi];
                        let sig = d.out.mux(infos[a].cond, cur.sig.clone(), t.sig.clone());
                        cur = Hw {
                            sig,
                            ty: t.ty.clone(),
                            tags: cur.tags.joined(&t.tags).joined(&sh.tags),
                        };
                    }
                    self.set_var(*k, n, cur);
                }
                Ok(())
            }
            StmtKind::For { var, start, end, inclusive, body } => {
                let a = self.eval_int(d, start)?;
                let mut b = self.eval_int(d, end)?;
                if *inclusive {
                    b = &b + &one();
                }
                if &b - &a > BigInt::from_u64(1 << 20) {
                    return d.err(
                        Diagnostic::error(st.span, "this loop runs more than 1048576 times")
                            .with_label("too many iterations"),
                    );
                }
                self.check_fresh(d, var)?;
                let mut i = a;
                while i < b {
                    let mut sc = Scope::default();
                    sc.syms.insert(var.name.clone(), Sym::Value(Value::Int(i.clone(), None)));
                    sc.decls.insert(var.name.clone(), var.span);
                    self.scopes.push(sc);
                    let r = self.exec(d, body);
                    self.scopes.pop();
                    r?;
                    i = &i + &one();
                }
                Ok(())
            }
            StmtKind::Assert(a) => self.check_assert(d, a),
            StmtKind::Block(b) => {
                self.scopes.push(Scope::default());
                let r = self.exec(d, b);
                self.scopes.pop();
                r
            }
            _ => {
                let kw = match &st.kind {
                    StmtKind::Wire { .. } => "wire",
                    StmtKind::Reg { .. } => "reg",
                    StmtKind::Mem { .. } => "mem",
                    StmtKind::Inst { .. } => "inst",
                    _ => "domain",
                };
                self.module_only(d, st, kw)
            }
        }
    }

    /// `var = value` with optional constant or runtime parts.
    fn assign_var(&mut self, d: &mut Design<'a>, target: &'a Expr, value: &'a Expr) -> R<()> {
        // Find the root variable and the path.
        let mut path: Vec<&'a Expr> = Vec::new();
        let mut e = target.strip();
        let root = loop {
            match &e.kind {
                ExprKind::Ident(id) => break id,
                ExprKind::Field(b, _) | ExprKind::Index(b, _) | ExprKind::Slice(b, _, _) => {
                    path.push(e);
                    e = b.strip();
                }
                _ => {
                    return d.err(
                        Diagnostic::error(target.span, "cannot assign to this expression")
                            .with_label("not a variable"),
                    );
                }
            }
        };
        if d.indexing
            && let Some(sym) =
                self.scopes.iter().rev().find_map(|s| s.syms.get(&root.name)).cloned()
        {
            self.note_sym(d, root, &sym);
        }
        let Some((scope, cur)) =
            self.scopes.iter().enumerate().rev().find_map(|(k, s)| match s.syms.get(&root.name) {
                Some(Sym::Var(h)) => Some((k, h.clone())),
                _ => None,
            })
        else {
            let known = self.scopes.iter().any(|s| s.syms.contains_key(&root.name));
            let mut dg =
                Diagnostic::error(root.span, format!("`{}` cannot be assigned", root.name));
            dg = if known {
                dg.with_label("not a `var`").with_help(format!(
                    "declare it with `var {}: <type> = ...;` to change it",
                    root.name
                ))
            } else {
                dg.with_label("not declared")
                    .with_help(format!("declare it first: `var {}: <type> = ...;`", root.name))
            };
            return d.err(dg);
        };
        // Positions: (condition, low bit), width, type.
        let mut alts: Vec<(SigBit, u32)> = vec![(SigBit::ONE, 0)];
        let mut ty = cur.ty.clone();
        let mut width = d.types.width(&ty);
        let mut tags = Tags::none();
        for p in path.iter().rev() {
            match &p.kind {
                ExprKind::Field(_, name) => {
                    let Ty::Struct(si) = ty else {
                        return d.err(
                            Diagnostic::error(name.span, "only structs have fields")
                                .with_label("no such field"),
                        );
                    };
                    let Some((off, w, t)) = self.field_layout(d, si, &name.name) else {
                        return d.err(
                            Diagnostic::error(name.span, format!("no field `{}`", name.name))
                                .with_label("unknown field"),
                        );
                    };
                    for a in &mut alts {
                        a.1 += off;
                    }
                    width = w;
                    ty = t;
                }
                ExprKind::Index(_, idx) => {
                    let (ew, n, et) = match &ty {
                        Ty::Array(et, n) => (d.types.width(et), *n, (**et).clone()),
                        Ty::Int(i) => (1, i.width() as u64, Ty::Bool),
                        _ => {
                            return d.err(
                                Diagnostic::error(p.span, "cannot index this")
                                    .with_label("not an array"),
                            );
                        }
                    };
                    match self.eval(d, idx, None)? {
                        Value::Int(k, _) => {
                            let Some(k) = k.to_u64().filter(|&k| k < n) else {
                                return d.err(
                                    Diagnostic::error(
                                        idx.span,
                                        format!(
                                            "index {k} is out of range: there are {n} elements"
                                        ),
                                    )
                                    .with_label("out of range"),
                                );
                            };
                            for a in &mut alts {
                                a.1 += k as u32 * ew;
                            }
                        }
                        other => {
                            let ih = self.to_hw(d, other, idx.span)?;
                            self.check_index_pub(
                                d,
                                &ih,
                                idx.span,
                                n,
                                &format!("`{}`", d.snippet(p.span)),
                            )?;
                            let Ty::Int(it) = &ih.ty else { unreachable!() };
                            let (lo, hi) = (
                                it.lo.to_u64().unwrap_or(0),
                                it.hi.to_u64().unwrap_or(n - 1).min(n - 1),
                            );
                            let mut na = Vec::new();
                            for (c, b) in &alts {
                                for k in lo..=hi {
                                    let kc = BigInt::from_u64(k).to_logic_vec(ih.sig.width());
                                    let eq = d.out.eq_const(ih.sig.clone(), &kc);
                                    na.push((d.out.and1(*c, eq), b + k as u32 * ew));
                                }
                            }
                            alts = na;
                            tags.join(&ih.tags);
                        }
                    }
                    width = ew;
                    ty = et;
                }
                ExprKind::Slice(_, h, l) => {
                    let h = self.eval_int(d, h)?.to_u64().unwrap_or(0);
                    let l = self.eval_int(d, l)?.to_u64().unwrap_or(0);
                    if l > h || h >= width as u64 || !matches!(ty, Ty::Int(_)) {
                        return d.err(
                            Diagnostic::error(p.span, "invalid slice").with_label("out of range"),
                        );
                    }
                    for a in &mut alts {
                        a.1 += l as u32;
                    }
                    width = (h - l + 1) as u32;
                    ty = Ty::Int(IntTy::decl(IntDecl::Uint(width)));
                }
                _ => unreachable!(),
            }
        }
        let h = self.eval_to(d, value, &ty, &format!("`{}`", d.snippet(target.span)))?;
        let mut sig = cur.sig.clone();
        for (c, lo) in alts {
            let old = sig.slice(lo as usize, width as usize);
            let new = d.out.mux(c, old, h.sig.clone());
            for k in 0..width as usize {
                sig.0[lo as usize + k] = new.0[k];
            }
        }
        let tags = cur.tags.joined(&h.tags).joined(&tags);
        // Keep the declared type of the variable.
        let mut nh = Hw { sig, ty: cur.ty.clone(), tags };
        if let Ty::Int(i) = &cur.ty {
            nh.sig = adjust(&nh.sig, i, i);
        }
        self.set_var(scope, &root.name, nh);
        let _ = konst;
        let _ = const_int;
        Ok(())
    }
}

/// Whether a type expression mentions any of `names`.
fn mentions(t: &ast::TypeExpr, names: &[String]) -> bool {
    use ast::TypeKind::*;
    fn expr(e: &Expr, names: &[String]) -> bool {
        match &e.kind {
            ExprKind::Ident(i) => names.contains(&i.name),
            ExprKind::Paren(x) | ExprKind::Unary(_, x) => expr(x, names),
            ExprKind::Binary(_, a, b) => expr(a, names) || expr(b, names),
            ExprKind::Call { args, .. } => args.iter().any(|a| expr(&a.value, names)),
            ExprKind::Field(x, _) => expr(x, names),
            _ => false,
        }
    }
    match &t.kind {
        Bool | Clock => false,
        Uint(e) | Sint(e) | Index(e) => expr(e, names),
        Array(t, e) => mentions(t, names) || expr(e, names),
        Named(n) => names.contains(&n.name),
    }
}

/// Binds generic parameters of a function from an argument type.
fn infer<'a>(d: &mut Design<'a>, frame: &mut ModCx<'a>, t: &'a ast::TypeExpr, arg: &Ty) {
    use ast::TypeKind::*;
    let bind = |frame: &mut ModCx<'a>, e: &'a Expr, v: u64| {
        if let ExprKind::Ident(n) = &e.strip().kind
            && !frame.generics.contains_key(&n.name)
        {
            frame.generics.insert(n.name.clone(), Value::Int(BigInt::from_u64(v), None));
        }
    };
    match (&t.kind, arg) {
        (Uint(e), Ty::Int(i)) if !i.signed() => {
            let w = match i.full_decl() {
                Some(IntDecl::Uint(n)) => *n,
                _ => i.width(),
            };
            bind(frame, e, w as u64);
        }
        (Sint(e), Ty::Int(i)) => bind(frame, e, i.width() as u64),
        (Index(e), Ty::Int(i)) => {
            if let Some(IntDecl::Index(n)) = &i.decl {
                bind(frame, e, n.to_u64().unwrap_or(1));
            }
        }
        (Array(et, e), Ty::Array(at, n)) => {
            bind(frame, e, *n);
            infer(d, frame, et, at);
        }
        (Named(n), _)
            if !frame.generics.contains_key(&n.name) && !d.type_names.contains_key(&n.name) =>
        {
            let ty = match arg {
                Ty::Int(i) if i.full_decl().is_none() => Ty::Int(IntTy::decl(i.natural_decl())),
                other => other.clone(),
            };
            frame.generics.insert(n.name.clone(), Value::Type(ty));
        }
        _ => {}
    }
}

fn decl_name_span(st: &Stmt) -> Span {
    match &st.kind {
        StmtKind::Const(c) => c.name.span,
        StmtKind::Let { name, .. }
        | StmtKind::Wire { name, .. }
        | StmtKind::Reg { name, .. }
        | StmtKind::Mem { name, .. }
        | StmtKind::Inst { name, .. }
        | StmtKind::Domain { name, .. } => name.span,
        _ => st.span,
    }
}

use crate::ast::Arg;
