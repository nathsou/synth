//! Elaboration of Verilog into a flat RTL [`Module`].
//!
//! Starting from the top module, parameters are evaluated, generate
//! constructs unrolled and the hierarchy flattened (instance names become
//! prefixes, `u_cpu.alu.x`). Continuous assignments and expressions become
//! RTL cells sized by the Verilog expression rules; `always` blocks are
//! lowered by symbolic execution ([`proc`]) into multiplexers, flip-flops,
//! latches and memory ports; `initial` blocks provide power-on values.
//!
//! Instances of modules without a definition are emitted as black-box
//! [`synth_rtl::Instance`] cells when the [`BlackBoxes`] library knows them
//! (vendor primitives).

mod expr;
mod proc;

use std::collections::HashMap;

use synth_core::{Diagnostic, Diagnostics, LogicVec, SourceMap, Span};
use synth_rtl::{Attr, Instance, MemId, Memory, Module, PortDir, Sig, SigBit, WireId};
use synth_verilog::ast;

pub use proc::read_mem_file;

/// Port knowledge for modules without a Verilog definition.
pub trait BlackBoxes {
    /// Direction of `port` of black box `module`; `None` if unknown.
    fn port_dir(&self, module: &str, port: &str) -> Option<PortDir>;
    /// Whether `module` is a known black box.
    fn is_blackbox(&self, module: &str) -> bool;
}

/// A library without black boxes.
#[derive(Debug, Default)]
pub struct NoBlackBoxes;

impl BlackBoxes for NoBlackBoxes {
    fn port_dir(&self, _: &str, _: &str) -> Option<PortDir> {
        None
    }
    fn is_blackbox(&self, _: &str) -> bool {
        false
    }
}

/// Elaboration options.
#[derive(Clone, Debug, Default)]
pub struct ElabOptions {
    /// Top module name; inferred when `None` (the only uninstantiated module).
    pub top: Option<String>,
    /// Parameter overrides for the top module.
    pub top_params: Vec<(String, LogicVec)>,
}

/// A wire-backed variable or net.
#[derive(Clone, Debug)]
pub(crate) struct VarInfo {
    pub wire: WireId,
    pub width: u32,
    pub signed: bool,
    /// Declared `[msb:lsb]` (`[0:0]` for scalars).
    pub msb: i64,
    pub lsb: i64,
    /// A `reg`/`integer` (procedurally assigned) rather than a net.
    #[allow(dead_code)]
    pub is_var: bool,
}

impl VarInfo {
    /// Storage bit of declared index `i`, if in range.
    pub fn bit_of(&self, i: i64) -> Option<u32> {
        let pos = if self.msb >= self.lsb { i - self.lsb } else { self.lsb - i };
        (pos >= 0 && pos < self.width as i64).then_some(pos as u32)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MemInfo {
    pub mem: MemId,
    pub width: u32,
    pub signed: bool,
    pub msb: i64,
    pub lsb: i64,
    /// Declared word range `[lo_decl:hi_decl]` as written.
    pub first: i64,
    pub last: i64,
}

impl MemInfo {
    pub fn word_bit(&self, i: i64) -> Option<u32> {
        let pos = if self.msb >= self.lsb { i - self.lsb } else { self.lsb - i };
        (pos >= 0 && pos < self.width as i64).then_some(pos as u32)
    }

    /// Lowest declared address.
    pub fn low(&self) -> i64 {
        self.first.min(self.last)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Sym<'a> {
    Var(VarInfo),
    Param {
        value: LogicVec,
        signed: bool,
    },
    Mem(MemInfo),
    Func(&'a ast::SubroutineDecl),
    Task(&'a ast::SubroutineDecl),
    /// A named generate block; the value is its name prefix.
    Block(String),
    Genvar,
}

#[derive(Default)]
pub(crate) struct Scope<'a> {
    pub names: HashMap<String, Sym<'a>>,
    /// Prefix for wires declared in this scope (`u_cpu.gen[3].`).
    pub prefix: String,
}

pub(crate) struct Elab<'a> {
    pub modules: HashMap<&'a str, &'a ast::Module>,
    pub sm: &'a SourceMap,
    pub out: Module,
    pub diags: Diagnostics,
    pub bb: &'a dyn BlackBoxes,
    pub scopes: Vec<Scope<'a>>,
    /// Names of generate blocks: prefix -> names declared inside.
    pub blocks: HashMap<String, HashMap<String, Sym<'a>>>,
    /// Power-on values of variable bits (from `initial` and initializers).
    pub init: HashMap<SigBit, synth_core::Logic>,
    /// Current procedural execution state, when lowering a process.
    pub proc: Option<proc::ProcState>,
    pub depth: usize,
    pub warned: std::collections::HashSet<String>,
    /// Counter for unique generated names.
    pub uniq: usize,
}

/// Elaborates `src` into a flat module.
pub fn elaborate(
    src: &ast::SourceText,
    sm: &SourceMap,
    opts: &ElabOptions,
    bb: &dyn BlackBoxes,
) -> (Option<Module>, Diagnostics) {
    let mut e = Elab {
        modules: HashMap::new(),
        sm,
        out: Module::new(""),
        diags: Diagnostics::default(),
        bb,
        scopes: Vec::new(),
        blocks: HashMap::new(),
        init: HashMap::new(),
        proc: None,
        depth: 0,
        warned: Default::default(),
        uniq: 0,
    };
    for m in &src.modules {
        if let Some(prev) = e.modules.insert(&m.name.name, m) {
            e.diags.push(
                Diagnostic::error(m.name.span, format!("module `{}` defined twice", m.name.name))
                    .with_note(Some(prev.name.span), "first definition here"),
            );
        }
    }
    let top = match &opts.top {
        Some(t) => match e.modules.get(t.as_str()) {
            Some(m) => *m,
            None => {
                e.diags.push(Diagnostic::global_error(format!("top module `{t}` not found")));
                return (None, e.diags);
            }
        },
        None => match find_top(src) {
            Ok(m) => m,
            Err(msg) => {
                e.diags.push(Diagnostic::global_error(msg));
                return (None, e.diags);
            }
        },
    };
    e.out.name = top.name.name.clone();
    let overrides: Vec<(String, (LogicVec, bool))> =
        opts.top_params.iter().map(|(k, v)| (k.clone(), (v.clone(), false))).collect();
    e.module(top, "", &overrides, &[], top.name.span, true);
    let mut m = std::mem::take(&mut e.out);
    if let Err(msg) = synth_rtl::opt::clean(&mut m) {
        e.diags.push(Diagnostic::global_error(msg));
    }
    if e.diags.has_errors() {
        return (None, e.diags);
    }
    (Some(m), e.diags)
}

/// The top module: the one marked `(* top *)`, else the single module not
/// instantiated by any other.
fn find_top(src: &ast::SourceText) -> Result<&ast::Module, String> {
    if let Some(m) = src.modules.iter().find(|m| m.attrs.iter().any(|a| a.name.name == "top")) {
        return Ok(m);
    }
    let mut instantiated = std::collections::HashSet::new();
    fn collect<'b>(items: &'b [ast::Item], set: &mut std::collections::HashSet<&'b str>) {
        for it in items {
            match it {
                ast::Item::Instance(i) => {
                    set.insert(i.module.name.as_str());
                }
                ast::Item::GenFor { body, .. } => collect(&body.items, set),
                ast::Item::GenIf { then, otherwise, .. } => {
                    collect(&then.items, set);
                    if let Some(o) = otherwise {
                        collect(&o.items, set);
                    }
                }
                ast::Item::GenCase { items, .. } => {
                    for c in items {
                        collect(&c.body.items, set);
                    }
                }
                ast::Item::GenBlock(b) => collect(&b.items, set),
                _ => {}
            }
        }
    }
    for m in &src.modules {
        collect(&m.items, &mut instantiated);
    }
    let tops: Vec<&ast::Module> =
        src.modules.iter().filter(|m| !instantiated.contains(m.name.name.as_str())).collect();
    match tops.as_slice() {
        [m] => Ok(m),
        [] => Err("no top module found".into()),
        many => Err(format!(
            "cannot infer the top module, candidates: {} (use --top)",
            many.iter().map(|m| m.name.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// A port connection of an instance being elaborated.
pub(crate) struct ConnBinding {
    pub name: String,
    /// Signal on the parent side: for inputs, the driving value; for
    /// outputs, the bits to drive. `None` = unconnected.
    pub sig: Option<Sig>,
    /// For outputs: the parent's lvalue bits.
    pub is_lvalue: bool,
    pub span: Span,
}

impl<'a> Elab<'a> {
    pub fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.diags.error(span, msg);
    }

    pub fn warn_once(&mut self, span: Span, msg: impl Into<String>) {
        let msg = msg.into();
        if self.warned.insert(msg.clone()) {
            self.diags.warning(span, msg);
        }
    }

    pub fn prefix(&self) -> String {
        self.scopes.last().map(|s| s.prefix.clone()).unwrap_or_default()
    }

    pub fn declare(&mut self, name: &str, sym: Sym<'a>, span: Span) {
        let scope = self.scopes.last_mut().unwrap();
        if let Some(old) = scope.names.get(name) {
            // Port declared again as reg/wire is fine (non-ANSI style): keep the wire.
            if matches!((old, &sym), (Sym::Var(_), Sym::Var(_))) {
                return;
            }
            if !matches!(old, Sym::Genvar) {
                self.diags.error(span, format!("`{name}` is already declared"));
                return;
            }
        }
        scope.names.insert(name.to_string(), sym);
    }

    pub fn lookup(&self, name: &str) -> Option<&Sym<'a>> {
        self.scopes.iter().rev().find_map(|s| s.names.get(name))
    }

    /// Looks up a hierarchical name like `blk.sig` or `gen[2].sig`.
    pub fn lookup_path(
        &mut self,
        parts: &[(ast::Ident, Option<Box<ast::Expr>>)],
    ) -> Option<Sym<'a>> {
        if parts.len() == 1 && parts[0].1.is_none() {
            return self.lookup(&parts[0].0.name).cloned();
        }
        let mut prefix: Option<String> = None;
        for (i, (id, idx)) in parts.iter().enumerate() {
            let last = i == parts.len() - 1;
            let key = match idx {
                Some(e) => {
                    let v = self.const_int(e)?;
                    format!("{}[{v}]", id.name)
                }
                None => id.name.clone(),
            };
            let sym = match &prefix {
                None => self.lookup(&key).cloned(),
                Some(p) => self.blocks.get(p).and_then(|names| names.get(&key)).cloned(),
            };
            match sym {
                Some(Sym::Block(p)) if !last => prefix = Some(p),
                Some(s) if last => return Some(s),
                _ => return None,
            }
        }
        None
    }

    pub fn new_wire(&mut self, name: &str, width: u32, signed: bool) -> WireId {
        let w = self.out.add_wire(name, width.max(1));
        self.out.wires[w].signed = signed;
        w
    }

    /// Evaluates a constant integer expression.
    pub fn const_int(&mut self, e: &ast::Expr) -> Option<i64> {
        let (v, signed) = self.const_expr(e)?;
        match v.to_i64(signed) {
            Some(i) => Some(i),
            None => {
                self.error(e.span(), "expression must be a known integer constant");
                None
            }
        }
    }

    /// Evaluates a constant expression to a value and signedness.
    pub fn const_expr(&mut self, e: &ast::Expr) -> Option<(LogicVec, bool)> {
        let (w, s) = self.expr_type(e);
        let saved = self.proc.take();
        let sig = self.emit(e, w, s);
        self.proc = saved;
        match sig.as_const() {
            Some(c) => Some((c, s)),
            None => {
                self.error(e.span(), "expression is not constant");
                None
            }
        }
    }

    /// Evaluates a range `[msb:lsb]` to integers.
    pub fn range(&mut self, r: &Option<ast::Range>) -> Option<(i64, i64)> {
        match r {
            None => Some((0, 0)),
            Some(r) => Some((self.const_int(&r.msb)?, self.const_int(&r.lsb)?)),
        }
    }

    /// Elaborates one module instance into the flat output.
    fn module(
        &mut self,
        m: &'a ast::Module,
        prefix: &str,
        params: &[(String, (LogicVec, bool))],
        conns: &[ConnBinding],
        inst_span: Span,
        is_top: bool,
    ) {
        if self.depth > 64 {
            self.error(inst_span, "module hierarchy too deep (recursive instantiation?)");
            return;
        }
        self.depth += 1;
        let saved_scopes = std::mem::take(&mut self.scopes);
        self.scopes.push(Scope { names: HashMap::new(), prefix: prefix.to_string() });

        // Header parameters.
        for p in &m.params {
            self.param_decl(p, params);
        }
        // Ports: ANSI declarations create wires immediately.
        let mut port_order: Vec<(String, Span)> = Vec::new();
        match &m.ports {
            ast::Ports::Ansi(decls) => {
                for d in decls {
                    for (n, init) in &d.names {
                        self.port_wire(d, n, init.as_ref());
                        port_order.push((n.name.clone(), n.span));
                    }
                }
            }
            ast::Ports::NonAnsi(names) => {
                for n in names {
                    port_order.push((n.name.clone(), n.span));
                }
            }
        }
        // Body: declarations first, then behaviour.
        self.items(&m.items, params);

        // Bind ports.
        let mut port_dirs: HashMap<String, (WireId, PortDir)> = HashMap::new();
        for (i, (name, span)) in port_order.iter().enumerate() {
            let Some(Sym::Var(v)) = self.lookup(name).cloned() else {
                self.error(*span, format!("port `{name}` has no declaration"));
                continue;
            };
            let Some((dir, _)) = self.out.wires[v.wire].port else {
                self.error(*span, format!("port `{name}` has no direction"));
                continue;
            };
            if is_top {
                self.out.wires[v.wire].port = Some((dir, i as u32));
            } else {
                self.out.wires[v.wire].port = None;
            }
            port_dirs.insert(name.clone(), (v.wire, dir));
        }
        if !is_top {
            for c in conns {
                let Some(&(w, dir)) = port_dirs.get(&c.name) else {
                    self.error(
                        c.span,
                        format!("module `{}` has no port `{}`", m.name.name, c.name),
                    );
                    continue;
                };
                let pw = self.out.wire_sig(w);
                match (dir, &c.sig) {
                    (_, None) => {}
                    (PortDir::Input, Some(s)) => {
                        let s = s.extend(pw.width(), false);
                        let s = Sig(s.0[..pw.width()].to_vec());
                        self.out.connect(pw, s);
                    }
                    (PortDir::Output | PortDir::Inout, Some(s)) => {
                        if !c.is_lvalue {
                            self.error(c.span, "output port must connect to a net or variable");
                            continue;
                        }
                        let n = s.width().min(pw.width());
                        if n > 0 {
                            self.out.connect(s.slice(0, n), pw.slice(0, n));
                        }
                    }
                }
            }
            // Unconnected inputs float (x); outputs simply stay internal.
        }
        self.scopes = saved_scopes;
        self.depth -= 1;
    }

    fn port_wire(&mut self, d: &ast::PortDecl, n: &ast::Ident, init: Option<&ast::Expr>) {
        let Some((msb, lsb)) = self.range(&d.range) else { return };
        let width = (msb - lsb).unsigned_abs() as u32 + 1;
        let (width, signed, msb, lsb) = match d.kind {
            Some(ast::NetKind::Integer) => (32, true, 31, 0),
            _ => (width, d.signed, msb, lsb),
        };
        let name = format!("{}{}", self.prefix(), n.name);
        let w = self.new_wire(&name, width, signed);
        let dir = match d.dir {
            ast::Direction::Input => PortDir::Input,
            ast::Direction::Output => PortDir::Output,
            ast::Direction::Inout => PortDir::Inout,
        };
        self.out.wires[w].port = Some((dir, 0));
        let is_var = d.kind.is_some_and(|k| k.is_variable());
        self.declare(
            &n.name,
            Sym::Var(VarInfo { wire: w, width, signed, msb, lsb, is_var }),
            n.span,
        );
        if let Some(e) = init {
            self.initializer(w, width, e);
        }
    }

    /// Records a declaration initializer as a power-on value.
    fn initializer(&mut self, w: WireId, width: u32, e: &ast::Expr) {
        if let Some((v, s)) = self.const_expr(e) {
            let v = v.resize(width as usize, s);
            for i in 0..width {
                self.init.insert(SigBit::Wire(w, i), v.get(i as usize));
            }
        }
    }

    pub fn param_decl(&mut self, p: &ast::ParamDecl, overrides: &[(String, (LogicVec, bool))]) {
        for (n, e) in &p.assigns {
            let overridden = if p.local {
                None
            } else {
                overrides.iter().find(|(k, _)| *k == n.name).map(|(_, v)| v.clone())
            };
            let (mut value, mut signed) = match overridden {
                Some(v) => v,
                None => match self.const_expr(e) {
                    Some(v) => v,
                    None => continue,
                },
            };
            // An explicit type or range fixes width and signedness.
            if p.kind == Some(ast::NetKind::Integer) {
                value = value.resize(32, signed);
                signed = true;
            } else if p.range.is_some() {
                if let Some((msb, lsb)) = self.range(&p.range) {
                    let w = (msb - lsb).unsigned_abs() as usize + 1;
                    value = value.resize(w, signed);
                    signed = p.signed;
                }
            } else if p.signed {
                signed = true;
            }
            self.declare(&n.name, Sym::Param { value, signed }, n.span);
        }
    }

    /// Elaborates the items of a module body or generate block.
    fn items(&mut self, items: &'a [ast::Item], overrides: &[(String, (LogicVec, bool))]) {
        // Pass 1: declarations.
        for it in items {
            match it {
                ast::Item::Param(p) => self.param_decl(p, overrides),
                ast::Item::Port(d) => {
                    for (n, init) in &d.names {
                        // A body port declaration may follow a reg/wire
                        // declaration of the same name or precede it.
                        if let Some(Sym::Var(v)) = self.lookup(&n.name).cloned() {
                            let dir = match d.dir {
                                ast::Direction::Input => PortDir::Input,
                                ast::Direction::Output => PortDir::Output,
                                ast::Direction::Inout => PortDir::Inout,
                            };
                            self.out.wires[v.wire].port = Some((dir, 0));
                        } else {
                            self.port_wire(d, n, init.as_ref());
                        }
                    }
                }
                ast::Item::Var(v) => self.var_decl(v),
                ast::Item::Genvar(names) => {
                    for n in names {
                        self.declare(&n.name, Sym::Genvar, n.span);
                    }
                }
                ast::Item::Function(f) => self.declare(&f.name.name, Sym::Func(f), f.name.span),
                ast::Item::Task(t) => self.declare(&t.name.name, Sym::Task(t), t.name.span),
                _ => {}
            }
        }
        // Initial blocks before processes, so flops get their power-on values.
        for it in items {
            if let ast::Item::Initial { body, .. } = it {
                self.initial(body);
            }
        }
        // Pass 2: behaviour.
        for it in items {
            match it {
                ast::Item::ContAssign { assigns, .. } => {
                    for (lhs, rhs) in assigns {
                        self.cont_assign(lhs, rhs);
                    }
                }
                ast::Item::Always { body, span, .. } => self.always(body, *span),
                ast::Item::Instance(inst) => self.instance(inst),
                ast::Item::Gate(g) => self.gate(g),
                ast::Item::GenFor { init, cond, step, body, span } => {
                    self.gen_for(init, cond, step, body, *span)
                }
                ast::Item::GenIf { cond, then, otherwise, .. } => {
                    if let Some(c) = self.const_expr(cond) {
                        if c.0.truthy() == Some(true) {
                            self.gen_block(then, None);
                        } else if let Some(o) = otherwise {
                            self.gen_block(o, None);
                        }
                    }
                }
                ast::Item::GenCase { expr, items: citems, .. } => {
                    let Some((v, _)) = self.const_expr(expr) else { continue };
                    let mut chosen = None;
                    'outer: for ci in citems {
                        for e in &ci.exprs {
                            if let Some((c, _)) = self.const_expr(e) {
                                let w = v.width().max(c.width());
                                if v.resize(w, false).case_eq(&c.resize(w, false)) {
                                    chosen = Some(&ci.body);
                                    break 'outer;
                                }
                            }
                        }
                    }
                    if chosen.is_none() {
                        chosen = citems.iter().find(|c| c.exprs.is_empty()).map(|c| &c.body);
                    }
                    if let Some(b) = chosen {
                        self.gen_block(b, None);
                    }
                }
                ast::Item::GenBlock(b) => self.gen_block(b, None),
                ast::Item::Defparam(_, span) => {
                    self.error(*span, "defparam is not supported; use parameter overrides")
                }
                _ => {}
            }
        }
    }

    fn var_decl(&mut self, v: &ast::VarDecl) {
        let Some((msb, lsb)) = self.range(&v.range) else { return };
        let (width, signed, msb, lsb) = match v.kind {
            ast::NetKind::Integer => (32, true, 31, 0),
            ast::NetKind::Time => (64, false, 63, 0),
            ast::NetKind::Real => {
                self.error(v.span, "real variables are not synthesizable");
                return;
            }
            _ => ((msb - lsb).unsigned_abs() as u32 + 1, v.signed, msb, lsb),
        };
        for d in &v.names {
            if !d.dims.is_empty() {
                self.memory_decl(v, d, width, signed, msb, lsb);
                continue;
            }
            if let Some(Sym::Var(_)) = self.lookup(&d.name.name)
                && self.scopes.last().unwrap().names.contains_key(&d.name.name)
            {
                // `output x; reg x;` — the port wire already exists.
                if let Some(Sym::Var(info)) =
                    self.scopes.last_mut().unwrap().names.get_mut(&d.name.name)
                {
                    info.is_var = v.kind.is_variable();
                    info.signed |= signed;
                    let w = info.wire;
                    if info.width != width {
                        // Non-ANSI: `output [7:0] q; reg [7:0] q;` widths must agree.
                        if v.range.is_some() {
                            let msg = format!(
                                "width of `{}` differs from its port declaration",
                                d.name.name
                            );
                            self.error(d.name.span, msg);
                        }
                    }
                    if let Some(e) = &d.init {
                        self.initializer(w, width, e);
                    }
                }
                continue;
            }
            let name = format!("{}{}", self.prefix(), d.name.name);
            let w = self.new_wire(&name, width, signed);
            let is_var = v.kind.is_variable();
            self.declare(
                &d.name.name,
                Sym::Var(VarInfo { wire: w, width, signed, msb, lsb, is_var }),
                d.name.span,
            );
            if let Some(e) = &d.init {
                if is_var {
                    self.initializer(w, width, e);
                } else {
                    // `wire x = expr;` is a continuous assignment.
                    let lhs = ast::Expr::Ident(vec![(d.name.clone(), None)], d.name.span);
                    self.cont_assign(&lhs, e);
                }
            }
            if v.kind == ast::NetKind::Supply0 || v.kind == ast::NetKind::Supply1 {
                let val = if v.kind == ast::NetKind::Supply1 {
                    Sig(vec![SigBit::ONE; width as usize])
                } else {
                    Sig::zeros(width as usize)
                };
                let s = self.out.wire_sig(w);
                self.out.connect(s, val);
            }
        }
    }

    fn memory_decl(
        &mut self,
        v: &ast::VarDecl,
        d: &ast::DeclName,
        width: u32,
        signed: bool,
        msb: i64,
        lsb: i64,
    ) {
        if d.dims.len() > 1 {
            self.error(d.name.span, "multi-dimensional arrays are not supported");
            return;
        }
        let r = Some(d.dims[0].clone());
        let Some((first, last)) = self.range(&r) else { return };
        let depth = (first - last).unsigned_abs() + 1;
        if depth > 1 << 24 {
            self.error(d.name.span, "memory too large");
            return;
        }
        let name = format!("{}{}", self.prefix(), d.name.name);
        let mem = self.out.memories.push(Memory {
            name,
            width,
            depth: depth as u32,
            offset: first.min(last),
            init: vec![LogicVec::xs(width as usize); depth as usize],
            reads: vec![],
            writes: vec![],
            alive: true,
        });
        let _ = v;
        self.declare(
            &d.name.name,
            Sym::Mem(MemInfo { mem, width, signed, msb, lsb, first, last }),
            d.name.span,
        );
    }

    fn cont_assign(&mut self, lhs: &ast::Expr, rhs: &ast::Expr) {
        let Some(targets) = self.lvalue(lhs, true) else { return };
        let width = targets.width() as u32;
        let (rw, rs) = self.expr_type(rhs);
        let value = self.emit(rhs, width.max(rw), rs);
        let value = Sig(value.0[..width as usize].to_vec());
        match targets {
            expr::Lvalue::Bits(bits) => self.out.connect(Sig(bits), value),
            expr::Lvalue::Dynamic { .. } | expr::Lvalue::Mem { .. } => self.error(
                lhs.span(),
                "continuous assignment target must be a net (no variable index or memory)",
            ),
        }
    }

    fn gen_block(&mut self, b: &'a ast::GenBlock, index: Option<i64>) {
        let name = b.name.as_ref().map(|n| match index {
            Some(i) => format!("{}[{i}]", n.name),
            None => n.name.clone(),
        });
        let prefix = match &name {
            Some(n) => format!("{}{}.", self.prefix(), n),
            None => match index {
                // Unnamed loop blocks still need unique names.
                Some(i) => format!("{}genblk[{i}].", self.prefix()),
                None => self.prefix(),
            },
        };
        self.scopes.push(Scope { names: HashMap::new(), prefix: prefix.clone() });
        self.items(&b.items, &[]);
        let scope = self.scopes.pop().unwrap();
        if let Some(n) = name {
            let full = prefix.trim_end_matches('.').to_string();
            self.blocks.insert(full.clone(), scope.names);
            let span = b.name.as_ref().unwrap().span;
            self.declare(&n, Sym::Block(full), span);
        }
    }

    fn gen_for(
        &mut self,
        init: &(ast::Ident, ast::Expr),
        cond: &ast::Expr,
        step: &(ast::Ident, ast::Expr),
        body: &'a ast::GenBlock,
        span: Span,
    ) {
        let var = &init.0.name;
        let Some(mut i) = self.const_int(&init.1) else { return };
        let mut count = 0;
        loop {
            // The genvar is visible as a 32-bit signed constant.
            self.scopes.push(Scope { names: HashMap::new(), prefix: self.prefix() });
            self.declare(
                var,
                Sym::Param { value: LogicVec::from_i64(i, 32), signed: true },
                init.0.span,
            );
            let c = self.const_expr(cond);
            if !matches!(c, Some((ref v, _)) if v.truthy() == Some(true)) {
                self.scopes.pop();
                break;
            }
            self.gen_block(body, Some(i));
            let next = self.const_int(&step.1);
            // Named blocks (`name[i]`) stay visible after the loop.
            let iter_scope = self.scopes.pop().unwrap();
            for (k, v) in iter_scope.names {
                if let Sym::Block(_) = v {
                    self.scopes.last_mut().unwrap().names.insert(k, v);
                }
            }
            match next {
                Some(n) => i = n,
                None => break,
            }
            count += 1;
            if count > 100_000 {
                self.error(span, "generate loop does not terminate");
                break;
            }
        }
    }

    fn instance(&mut self, decl: &'a ast::InstanceDecl) {
        let name = &decl.module.name;
        let def = self.modules.get(name.as_str()).copied();
        if def.is_none() && !self.bb.is_blackbox(name) {
            self.error(decl.module.span, format!("unknown module `{name}`"));
            return;
        }
        for inst in &decl.instances {
            if inst.range.is_some() {
                self.error(inst.span, "instance arrays are not supported");
                continue;
            }
            match def {
                Some(m) => self.user_instance(m, decl, inst),
                None => self.blackbox_instance(decl, inst),
            }
        }
    }

    fn param_overrides(
        &mut self,
        m: Option<&ast::Module>,
        decl: &ast::InstanceDecl,
    ) -> Vec<(String, (LogicVec, bool))> {
        let mut out = Vec::new();
        match &decl.params {
            None => {}
            Some(ast::ParamOverride::Named(v)) => {
                for (n, e) in v {
                    if let Some(e) = e
                        && let Some(val) = self.const_expr(e)
                    {
                        out.push((n.name.clone(), val));
                    }
                }
            }
            Some(ast::ParamOverride::Ordered(v)) => {
                let names: Vec<String> = match m {
                    Some(m) => {
                        let mut names: Vec<String> = m
                            .params
                            .iter()
                            .filter(|p| !p.local)
                            .flat_map(|p| p.assigns.iter().map(|(n, _)| n.name.clone()))
                            .collect();
                        if names.is_empty() {
                            for it in &m.items {
                                if let ast::Item::Param(p) = it
                                    && !p.local
                                {
                                    names.extend(p.assigns.iter().map(|(n, _)| n.name.clone()));
                                }
                            }
                        }
                        names
                    }
                    None => {
                        self.error(
                            decl.span,
                            "ordered parameter overrides need a module definition",
                        );
                        return out;
                    }
                };
                for (i, e) in v.iter().enumerate() {
                    let Some(n) = names.get(i) else {
                        self.error(e.span(), "too many parameter overrides");
                        break;
                    };
                    if let Some(val) = self.const_expr(e) {
                        out.push((n.clone(), val));
                    }
                }
            }
        }
        out
    }

    fn user_instance(
        &mut self,
        m: &'a ast::Module,
        decl: &ast::InstanceDecl,
        inst: &ast::Instance,
    ) {
        let params = self.param_overrides(Some(m), decl);
        // Port names in order, for ordered connections.
        let port_names: Vec<String> = match &m.ports {
            ast::Ports::Ansi(d) => {
                d.iter().flat_map(|d| d.names.iter().map(|(n, _)| n.name.clone())).collect()
            }
            ast::Ports::NonAnsi(n) => n.iter().map(|n| n.name.clone()).collect(),
        };
        let port_dir = |pname: &str| -> Option<ast::Direction> {
            if let ast::Ports::Ansi(d) = &m.ports {
                for d in d {
                    if d.names.iter().any(|(n, _)| n.name == pname) {
                        return Some(d.dir);
                    }
                }
            }
            for it in &m.items {
                if let ast::Item::Port(d) = it
                    && d.names.iter().any(|(n, _)| n.name == pname)
                {
                    return Some(d.dir);
                }
            }
            None
        };
        let mut conns = Vec::new();
        for (i, c) in inst.conns.iter().enumerate() {
            let (pname, expr, span) = match c {
                ast::PortConn::Ordered(e) => {
                    let Some(n) = port_names.get(i) else {
                        self.error(inst.span, "too many port connections");
                        break;
                    };
                    (n.clone(), e.as_ref(), e.as_ref().map_or(inst.span, |e| e.span()))
                }
                ast::PortConn::Named(n, e) => (n.name.clone(), e.as_ref(), n.span),
                ast::PortConn::Wildcard(s) => {
                    self.error(*s, "`.*` connections are not supported");
                    continue;
                }
            };
            let dir = port_dir(&pname);
            let Some(e) = expr else {
                conns.push(ConnBinding { name: pname, sig: None, is_lvalue: false, span });
                continue;
            };
            match dir {
                Some(ast::Direction::Input) | None => {
                    let (w, s) = self.expr_type(e);
                    let sig = self.emit(e, w, s);
                    conns.push(ConnBinding { name: pname, sig: Some(sig), is_lvalue: false, span });
                }
                Some(_) => {
                    let lv = self.lvalue(e, true);
                    match lv {
                        Some(expr::Lvalue::Bits(b)) => conns.push(ConnBinding {
                            name: pname,
                            sig: Some(Sig(b)),
                            is_lvalue: true,
                            span,
                        }),
                        Some(_) => self.error(span, "output port must connect to a net"),
                        None => {}
                    }
                }
            }
        }
        let prefix = format!("{}{}.", self.prefix(), inst.name.name);
        self.module(m, &prefix, &params, &conns, inst.span, false);
    }

    fn blackbox_instance(&mut self, decl: &ast::InstanceDecl, inst: &ast::Instance) {
        let module = decl.module.name.clone();
        let mut params = Vec::new();
        match &decl.params {
            Some(ast::ParamOverride::Named(v)) => {
                for (n, e) in v {
                    let Some(e) = e else { continue };
                    let a = match e {
                        ast::Expr::Str(s, _) => Attr::Str(s.clone()),
                        _ => match self.const_expr(e) {
                            Some((v, _)) => Attr::Const(v),
                            None => continue,
                        },
                    };
                    params.push((n.name.clone(), a));
                }
            }
            Some(ast::ParamOverride::Ordered(_)) => {
                self.error(
                    decl.span,
                    format!("black box `{module}` needs named parameter overrides"),
                );
            }
            None => {}
        }
        let mut conns = Vec::new();
        for c in &inst.conns {
            let ast::PortConn::Named(n, e) = c else {
                self.error(inst.span, format!("black box `{module}` needs named port connections"));
                return;
            };
            let Some(dir) = self.bb.port_dir(&module, &n.name) else {
                self.error(n.span, format!("`{module}` has no port `{}`", n.name));
                continue;
            };
            let Some(e) = e else { continue };
            match dir {
                PortDir::Input => {
                    let (w, s) = self.expr_type(e);
                    let sig = self.emit(e, w, s);
                    conns.push((n.name.clone(), dir, sig));
                }
                _ => match self.lvalue(e, true) {
                    Some(expr::Lvalue::Bits(b)) => {
                        // The black box drives fresh bits that connect to the target.
                        let fresh = self.out.new_sig(b.len());
                        self.out.connect(Sig(b), fresh.clone());
                        conns.push((n.name.clone(), dir, fresh));
                    }
                    Some(_) => self.error(e.span(), "black box output must connect to a net"),
                    None => {}
                },
            }
        }
        let name = format!("{}{}", self.prefix(), inst.name.name);
        self.out.add_cell(
            name,
            synth_rtl::CellKind::Instance(Box::new(Instance { module, params, conns })),
        );
    }

    fn gate(&mut self, g: &ast::GateInstance) {
        use synth_rtl::{BinaryOp, UnaryOp};
        if g.conns.len() < 2 {
            self.error(g.span, "gate needs an output and at least one input");
            return;
        }
        let Some(expr::Lvalue::Bits(out)) = self.lvalue(&g.conns[0], true) else { return };
        let mut ins = Vec::new();
        for e in &g.conns[1..] {
            let s = self.emit(e, 1, false);
            ins.push(s);
        }
        let mut acc = ins[0].clone();
        let op = match g.kind.as_str() {
            "and" | "nand" => Some(BinaryOp::And),
            "or" | "nor" => Some(BinaryOp::Or),
            "xor" | "xnor" => Some(BinaryOp::Xor),
            _ => None,
        };
        if let Some(op) = op {
            for s in &ins[1..] {
                acc = self.out.binary(op, false, acc, s.clone());
            }
        }
        if matches!(g.kind.as_str(), "nand" | "nor" | "xnor" | "not") {
            acc = self.out.unary(UnaryOp::Not, acc);
        }
        let acc = acc.extend(out.len(), false);
        self.out.connect(Sig(out), acc);
    }

    pub fn fresh(&mut self, base: &str) -> String {
        self.uniq += 1;
        format!("{}${base}{}", self.prefix(), self.uniq)
    }
}

#[cfg(test)]
mod tests;
