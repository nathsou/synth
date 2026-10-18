//! Elaboration: checking and lowering Weft into one flat RTL module.
//!
//! Each module instance is elaborated with concrete generic arguments (as
//! templates are), which keeps checking and lowering in one pass. Module-level
//! declarations resolve lazily, so they may be used before they appear.
//! Assignments are collected as drivers and turned into multiplexer chains
//! once the whole body has been seen; timing tags are checked last, when the
//! tags of all wires are known.

mod expr;
mod finish;
mod index;
mod inline;
mod pattern;
mod stmt;

use std::collections::{HashMap, HashSet};

use synth_core::{BigInt, Diagnostic, Diagnostics, FileId, LogicVec, Severity, SourceMap, Span};
use synth_rtl::{Module, PortDir, Sig, SigBit, WireId};

use crate::ast::{self, GenArgValue, GenericKind, ItemKind, PortKind, StmtKind, TypeKind};
use crate::types::*;

pub(crate) type R<T> = Result<T, ()>;
pub use index::{RefKind, Reference};

/// Elaboration options.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Top module; inferred when `None`.
    pub top: Option<String>,
    /// Generic arguments of the top module: integers or `"strings"`.
    pub params: Vec<(String, String)>,
    /// Check every module, without requiring a top (for editors).
    pub check_all: bool,
}

/// A clock value.
#[derive(Clone, Debug)]
pub(crate) struct ClockVal {
    pub id: ClockId,
    pub bit: SigBit,
    pub origin: ClockOrigin,
}

/// Where a module's clock comes from, seen from its parent.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ClockOrigin {
    /// A clock input port of the module.
    Port(String),
    /// A domain parameter (named, or `default` for the implicit one).
    Param(String),
    /// Created inside the module (an instance output).
    Internal,
}

#[derive(Clone, Debug)]
pub(crate) struct Hw {
    pub sig: Sig,
    pub ty: Ty,
    pub tags: Tags,
}

#[derive(Clone, Debug)]
pub(crate) enum Value {
    Hw(Hw),
    Int(BigInt, Option<u32>),
    Str(String),
    Clock(ClockVal),
    Domain(usize),
    Inst(usize),
    Mem(usize),
    Type(Ty),
}

#[derive(Clone, Debug)]
pub(crate) enum Sym {
    Value(Value),
    /// A mutable function local.
    Var(Hw),
    Signal(usize),
    Domain(usize),
    /// A clock input port, also a domain.
    ClockPort(usize),
    Inst(usize),
    Mem(usize),
}

#[derive(Clone, Debug)]
pub(crate) enum SigKind {
    In,
    Out,
    Wire,
    Reg { dom: usize, has_reset: bool, value: LogicVec },
    InstIn { inst: usize, default: Option<Sig> },
    InstOut { inst: usize },
}

/// The fixed tag of a signal: the atom it carries when read and the clock
/// it accepts when driven (`None` accepts anything).
#[derive(Clone, Debug)]
pub(crate) struct FixedTag {
    pub atom: Option<TagAtom>,
    pub what: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Signal {
    pub name: String,
    pub span: Span,
    pub kind: SigKind,
    pub ty: Ty,
    pub sig: Sig,
    /// `None` for wires whose tag is inferred from their drivers.
    pub tag: Option<FixedTag>,
    pub read: bool,
    pub assigned: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Driver {
    pub sig: usize,
    pub lo: u32,
    pub width: u32,
    pub value: Sig,
    pub guard: SigBit,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub(crate) struct DomainInfo {
    pub name: String,
    pub clock: ClockVal,
    pub posedge: bool,
    /// Active-high reset bit, and whether it is asynchronous.
    pub reset: Option<(SigBit, bool)>,
}

#[derive(Clone, Debug)]
pub(crate) struct InstInfo<'a> {
    pub name: String,
    pub module: String,
    pub span: Span,
    pub ports: Vec<(String, usize)>,
    pub has_outputs: bool,
    /// Connections from the instantiation, driven where it is declared.
    pub pending: Vec<(usize, &'a ast::Expr)>,
    /// Clock outputs of extern modules.
    pub clocks: Vec<(String, ClockVal)>,
}

#[derive(Clone, Debug)]
pub(crate) struct MemInfo {
    pub name: String,
    pub span: Span,
    pub elem: Ty,
    pub depth: u64,
    pub dom: usize,
    pub id: synth_rtl::MemId,
    pub read: bool,
    pub written: bool,
    pub has_init: bool,
    pub comb_reads: Vec<Span>,
}

#[derive(Clone, Debug)]
pub(crate) struct Sink {
    /// The clock this sink samples on (async sinks are not recorded).
    pub clock: ClockId,
    pub tags: Tags,
    pub span: Span,
    pub target: String,
}

pub(crate) enum Lazy<'a> {
    Pending(&'a ast::Stmt),
    Busy(Span),
    Done(Sym),
    Failed,
}

#[derive(Default)]
pub(crate) struct Scope {
    pub syms: HashMap<String, Sym>,
    /// Where the names of this scope are declared (for the editor index).
    pub decls: HashMap<String, Span>,
    pub narrow: HashMap<String, IntTy>,
}

/// How the parent binds one of a child's domain parameters.
#[derive(Clone, Debug)]
pub(crate) struct DomBinding {
    pub clk: SigBit,
    pub posedge: bool,
    pub reset: Option<(SigBit, bool)>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Module,
    Function,
}

/// Per-instance (or per-call, for functions) elaboration state.
pub(crate) struct ModCx<'a> {
    pub def_name: String,
    pub body: &'a [ast::Stmt],
    pub ports: &'a [ast::Port],
    pub mode: Mode,
    pub prefix: String,
    pub generics: HashMap<String, Value>,
    pub is_top: bool,
    pub scopes: Vec<Scope>,
    pub lazy: HashMap<String, Lazy<'a>>,
    pub signals: Vec<Signal>,
    pub drivers: Vec<Driver>,
    pub domains: Vec<DomainInfo>,
    /// The implicit domain parameter's index, if the module has one.
    pub implicit_dom: Option<usize>,
    pub default_dom: Option<usize>,
    pub default_name: Option<String>,
    pub insts: Vec<InstInfo<'a>>,
    pub mems: Vec<MemInfo>,
    pub sinks: Vec<Sink>,
    pub var_sources: HashMap<usize, Tags>,
    /// Paths on which a signal was assigned in one branch only.
    pub partial: Vec<(usize, Span, String)>,
    pub loop_names: Vec<String>,
    pub allow_module_call: bool,
    pub let_names: Vec<String>,
    pub lets_read: HashMap<String, (Span, bool)>,
    pub uniq: usize,
    pub file: FileId,
    pub is_extern: bool,
    /// Set while evaluating the value of a register assignment.
    pub reg_read: bool,
    /// Name for instances created by module calls in a `let`.
    pub call_name: Option<String>,
    pub top_defs: stmt::DefSet,
    pub dead_warned: HashSet<usize>,
    /// Instance inputs connected in their instantiation.
    pub connected: HashSet<usize>,
    /// Bits assigned unconditionally by instance connections.
    pub conn_defs: stmt::DefSet,
    pub let_used: HashSet<String>,
    /// Where generic parameters are declared (for the editor index).
    pub generic_decls: HashMap<String, Span>,
    /// Set when the body had errors (completeness is then not checked).
    pub walk_failed: bool,
}

pub(crate) struct Design<'a> {
    pub sm: &'a SourceMap,
    pub quiet_files: HashSet<FileId>,
    pub prim_files: HashSet<FileId>,
    pub diags: Diagnostics,
    seen: HashSet<(Option<Span>, String)>,
    pub modules: HashMap<String, &'a ast::Module>,
    pub fns: HashMap<String, &'a ast::Function>,
    pub consts: HashMap<String, &'a ast::ConstDecl>,
    pub const_cache: HashMap<String, Option<Value>>,
    pub asserts: Vec<&'a ast::Assert>,
    pub types: TypeDefs,
    pub type_names: HashMap<String, Ty>,
    /// Definitions by `Ty::Enum` and `Ty::Struct` index.
    pub enum_defs: Vec<&'a ast::EnumDef>,
    pub struct_defs: Vec<&'a ast::StructDef>,
    pub out: Module,
    pub verilog: Option<&'a synth_verilog::ast::SourceText>,
    pub next_clock: ClockId,
    pub clock_names: HashMap<ClockId, String>,
    /// Clocks of domains that have none (a clockless top module's).
    pub clockless: HashSet<ClockId>,
    pub elaborated: HashSet<String>,
    pub depth: usize,
    pub fn_depth: usize,
    pub wire_info: HashMap<WireId, (String, Span)>,
    /// Whether to record references (editors only).
    pub indexing: bool,
    pub refs: Vec<Reference>,
    noted: HashSet<Span>,
}

impl<'a> Design<'a> {
    pub fn emit(&mut self, d: Diagnostic) {
        if d.severity == Severity::Warning
            && let Some(sp) = d.span
            && self.quiet_files.contains(&sp.file)
        {
            return;
        }
        if self.seen.insert((d.span, d.message.clone())) {
            self.diags.push(d);
        }
    }

    pub fn err<T>(&mut self, d: Diagnostic) -> R<T> {
        self.emit(d);
        Err(())
    }

    pub fn fresh_clock(&mut self, name: impl Into<String>) -> ClockId {
        let id = self.next_clock;
        self.next_clock += 1;
        self.clock_names.insert(id, name.into());
        id
    }

    pub fn snippet(&self, sp: Span) -> String {
        let s = self.sm.snippet(sp);
        let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if s.chars().count() > 48 {
            format!("{}…", s.chars().take(47).collect::<String>())
        } else {
            s
        }
    }

    pub fn add_wire(&mut self, name: &str, width: u32, span: Span, public: bool) -> Sig {
        let w = self.out.add_wire(name, width.max(1));
        self.out.wires[w].public = public;
        if public {
            self.wire_info.insert(w, (name.to_string(), span));
        }
        Sig::wire(w, width.max(1))
    }
}

/// Closest candidate by edit distance, for "did you mean" suggestions.
pub(crate) fn suggest<'s>(name: &str, cands: impl IntoIterator<Item = &'s str>) -> Option<String> {
    let lim = (name.len() / 3).max(1);
    cands
        .into_iter()
        .filter(|c| *c != name && !c.starts_with('$'))
        .map(|c| (edit_distance(name, c), c))
        .filter(|(d, _)| *d <= lim)
        .min()
        .map(|(_, c)| c.to_string())
}

/// Edit distance where swapping two adjacent characters counts as one edit.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let c = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + c);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// Elaborates parsed files into a flat module. `quiet` files (the standard
/// library and preludes) produce no warnings; externs declared in `prims`
/// files become primitive instances.
pub(crate) fn elaborate(
    sm: &SourceMap,
    files: &[(FileId, &ast::File)],
    quiet: &[FileId],
    prims: &[FileId],
    verilog: Option<&synth_verilog::ast::SourceText>,
    opts: &Options,
) -> (Option<Module>, Diagnostics, Vec<Reference>) {
    let mut d = Design {
        sm,
        quiet_files: quiet.iter().copied().collect(),
        prim_files: prims.iter().copied().collect(),
        diags: Diagnostics::default(),
        seen: HashSet::new(),
        modules: HashMap::new(),
        fns: HashMap::new(),
        consts: HashMap::new(),
        const_cache: HashMap::new(),
        asserts: Vec::new(),
        types: TypeDefs::default(),
        type_names: HashMap::new(),
        out: Module::new("top"),
        verilog,
        next_clock: 0,
        clock_names: HashMap::new(),
        clockless: HashSet::new(),
        elaborated: HashSet::new(),
        depth: 0,
        fn_depth: 0,
        wire_info: HashMap::new(),
        indexing: opts.check_all,
        enum_defs: Vec::new(),
        struct_defs: Vec::new(),
        refs: Vec::new(),
        noted: HashSet::new(),
    };
    let top = collect(&mut d, files, opts);
    if top.is_none() && !opts.check_all {
        return (None, d.diags, Vec::new());
    }
    if !d.diags.has_errors() {
        check_global_asserts(&mut d);
    }
    let ok = match &top {
        Some(top) => {
            let def = d.modules[top];
            d.out.name = top.clone();
            elaborate_top(&mut d, def, opts).is_ok()
        }
        None => false,
    };
    // Check the modules the top does not use.
    let mut unused: Vec<&ast::Module> = d
        .modules
        .values()
        .filter(|m| {
            m.body.is_some()
                && !d.elaborated.contains(&m.name.name)
                && !d.quiet_files.contains(&m.name.span.file)
                && m.generics.iter().all(|g| g.default.is_some())
        })
        .copied()
        .collect();
    unused.sort_by_key(|m| (m.name.span.file, m.name.span.start));
    let main = std::mem::replace(&mut d.out, Module::new("scratch"));
    for m in unused {
        let _ = check_standalone(&mut d, m);
    }
    d.out = main;
    if d.indexing {
        index_items(&mut d, files);
    }
    if ok && !d.diags.has_errors() {
        crate::check::combinational_cycles(&mut d);
        crate::check::reset_loops(&mut d);
    }
    let failed = d.diags.has_errors();
    let mut diags = d.diags;
    diags.items.sort_by_key(|x| {
        (x.span.map(|s| (s.file, s.start)).unwrap_or_default(), std::cmp::Reverse(x.severity))
    });
    let refs = std::mem::take(&mut d.refs);
    (if failed || top.is_none() { None } else { Some(d.out) }, diags, refs)
}

/// Records the names of top-level items at their definitions.
fn index_items<'a>(d: &mut Design<'a>, files: &[(FileId, &'a ast::File)]) {
    for (_, f) in files {
        for it in &f.items {
            match &it.kind {
                ItemKind::Module(m) => {
                    d.note(m.name.span, Some(m.name.span), RefKind::Module, d.module_hover(m))
                }
                ItemKind::Function(func) => d.note(
                    func.name.span,
                    Some(func.name.span),
                    RefKind::Function,
                    d.fn_hover(func),
                ),
                ItemKind::Struct(s) => {
                    if let Some(t) = d.type_names.get(&s.name.name).cloned() {
                        d.note(s.name.span, Some(s.name.span), RefKind::Type, d.type_hover(&t));
                        if let Ty::Struct(si) = t {
                            for (n, _) in &s.fields {
                                let Some((_, ft)) =
                                    d.types.structs[si].fields.iter().find(|(f, _)| *f == n.name)
                                else {
                                    continue;
                                };
                                let hover =
                                    index::code(&format!("{}: {}", n.name, d.types.name(ft)))
                                        + &format!(
                                            "\n\nfield of `{}`, {}",
                                            s.name.name,
                                            index::bits(d.types.width(ft))
                                        );
                                d.note(n.span, Some(n.span), RefKind::Field, hover);
                            }
                        }
                    }
                }
                ItemKind::Enum(e) => {
                    if let Some(t) = d.type_names.get(&e.name.name).cloned() {
                        d.note(e.name.span, Some(e.name.span), RefKind::Type, d.type_hover(&t));
                        if let Ty::Enum(ei) = t {
                            for (n, _) in &e.variants {
                                let Some((_, c)) =
                                    d.types.enums[ei].variants.iter().find(|(v, _)| *v == n.name)
                                else {
                                    continue;
                                };
                                let hover =
                                    index::code(&format!("{}.{} = {c}", e.name.name, n.name));
                                d.note(n.span, Some(n.span), RefKind::Variant, hover);
                            }
                        }
                    }
                }
                ItemKind::Const(c) => {
                    if let Some(Some(v)) = d.const_cache.get(&c.name.name).cloned() {
                        let hover = ModCx::global().describe_value(d, &c.name.name, &v);
                        d.note(c.name.span, Some(c.name.span), RefKind::Const, hover);
                    }
                }
                ItemKind::Assert(_) => {}
            }
        }
    }
}

/// Records items, checks names, resolves type definitions and picks the top.
fn collect<'a>(
    d: &mut Design<'a>,
    files: &[(FileId, &'a ast::File)],
    opts: &Options,
) -> Option<String> {
    let mut first: HashMap<String, Span> = HashMap::new();
    let mut enums = Vec::new();
    let mut structs = Vec::new();
    for (_, f) in files {
        for it in &f.items {
            let name = match &it.kind {
                ItemKind::Module(m) => &m.name,
                ItemKind::Function(f) => &f.name,
                ItemKind::Struct(s) => &s.name,
                ItemKind::Enum(e) => &e.name,
                ItemKind::Const(c) => &c.name,
                ItemKind::Assert(a) => {
                    d.asserts.push(a);
                    continue;
                }
            };
            if let Some(prev) = first.get(&name.name) {
                d.emit(
                    Diagnostic::error(name.span, format!("`{}` is defined twice", name.name))
                        .with_label("second definition")
                        .with_secondary(*prev, "first defined here"),
                );
                continue;
            }
            if crate::BUILTINS.contains(&name.name.as_str()) {
                d.emit(
                    Diagnostic::error(
                        name.span,
                        format!("`{}` is a builtin and cannot be redefined", name.name),
                    )
                    .with_label("builtin name"),
                );
                continue;
            }
            first.insert(name.name.clone(), name.span);
            match &it.kind {
                ItemKind::Module(m) => {
                    d.modules.insert(m.name.name.clone(), m);
                }
                ItemKind::Function(f) => {
                    d.fns.insert(f.name.name.clone(), f);
                }
                ItemKind::Struct(s) => {
                    d.type_names.insert(s.name.name.clone(), Ty::Struct(structs.len()));
                    structs.push(s);
                    d.struct_defs.push(s);
                    d.types.structs.push(StructInfo { name: s.name.name.clone(), fields: vec![] });
                }
                ItemKind::Enum(e) => {
                    d.type_names.insert(e.name.name.clone(), Ty::Enum(enums.len()));
                    enums.push(e);
                    d.enum_defs.push(e);
                    d.types.enums.push(EnumInfo {
                        name: e.name.name.clone(),
                        variants: vec![],
                        width: 1,
                        explicit: false,
                        span: e.name.span,
                    });
                }
                ItemKind::Const(c) => {
                    d.consts.insert(c.name.name.clone(), c);
                }
                ItemKind::Assert(_) => unreachable!(),
            }
        }
    }
    // Type definitions.
    for (i, e) in enums.iter().enumerate() {
        let _ = define_enum(d, i, e);
    }
    // Struct fields may name other structs: resolve in dependency order.
    let mut done = vec![false; structs.len()];
    for i in 0..structs.len() {
        let _ = define_struct(d, &structs, i, &mut done, &mut Vec::new());
    }
    // Top module.
    let top = match &opts.top {
        Some(t) => match d.modules.get(t.as_str()) {
            Some(m) if m.body.is_some() => t.clone(),
            Some(m) => {
                d.emit(
                    Diagnostic::error(m.name.span, format!("top module `{t}` is an extern module"))
                        .with_label("declared here without a body"),
                );
                return None;
            }
            None => {
                let names: Vec<&str> = d.modules.keys().map(|s| s.as_str()).collect();
                let mut dg = Diagnostic::global_error(format!("top module `{t}` not found"));
                if let Some(s) = suggest(t, names) {
                    dg = dg.with_help(format!("did you mean `{s}`?"));
                }
                d.emit(dg);
                return None;
            }
        },
        None => {
            let mut used = HashSet::new();
            for m in d.modules.values() {
                if let Some(b) = &m.body {
                    collect_calls(b, &mut used);
                }
            }
            let mut cands: Vec<&ast::Module> = d
                .modules
                .values()
                .filter(|m| {
                    m.body.is_some()
                        && !used.contains(&m.name.name)
                        && !d.quiet_files.contains(&m.name.span.file)
                })
                // A module whose generic parameters need arguments cannot be the top.
                .filter(|m| {
                    m.generics.iter().all(|g| {
                        g.default.is_some() || opts.params.iter().any(|(k, _)| *k == g.name.name)
                    }) && opts
                        .params
                        .iter()
                        .all(|(k, _)| m.generics.iter().any(|g| g.name.name == *k))
                })
                .copied()
                .collect();
            cands.sort_by_key(|m| (m.name.span.file, m.name.span.start));
            match cands.len() {
                1 => cands[0].name.name.clone(),
                _ if opts.check_all => return None,
                0 => {
                    d.emit(
                        Diagnostic::global_error(
                            "no top module: every module is instantiated by another",
                        )
                        .with_help("choose one with `--top <module>`"),
                    );
                    return None;
                }
                _ => {
                    let names: Vec<String> =
                        cands.iter().map(|m| format!("`{}`", m.name.name)).collect();
                    let mut dg = Diagnostic::global_error(format!(
                        "cannot tell which module is the top: {} are not instantiated by any other module",
                        names.join(", ")
                    ))
                    .with_help("choose one with `--top <module>`");
                    for m in cands.iter().take(4) {
                        dg = dg.with_note(
                            Some(m.name.span),
                            format!("`{}` is a candidate", m.name.name),
                        );
                    }
                    d.emit(dg);
                    return None;
                }
            }
        }
    };
    Some(top)
}

fn collect_calls(stmts: &[ast::Stmt], used: &mut HashSet<String>) {
    fn expr(e: &ast::Expr, used: &mut HashSet<String>) {
        use ast::ExprKind::*;
        match &e.kind {
            Call { callee, args, .. } => {
                used.insert(callee.name.clone());
                for a in args {
                    expr(&a.value, used);
                }
            }
            Paren(x) | Field(x, _) | Unary(_, x) | Convert(_, x) => expr(x, used),
            Method(x, _, a) => {
                expr(x, used);
                for a in a {
                    expr(&a.value, used);
                }
            }
            Index(a, b) | Binary(_, a, b) | ArrayRepeat(a, b) => {
                expr(a, used);
                expr(b, used);
            }
            Slice(a, b, c) | PartSelect(a, b, c) => {
                expr(a, used);
                expr(b, used);
                expr(c, used);
            }
            If(c, t, e) => {
                expr(c, used);
                expr(&t.value, used);
                expr(e, used);
            }
            Block(b) => expr(&b.value, used),
            Match(s, arms) => {
                expr(s, used);
                for (_, v) in arms {
                    expr(v, used);
                }
            }
            Struct(_, f) => {
                for (_, v) in f {
                    expr(v, used);
                }
            }
            Array(items) => {
                for i in items {
                    expr(i, used);
                }
            }
            _ => {}
        }
    }
    for s in stmts {
        match &s.kind {
            StmtKind::Inst { call, .. } => expr(call, used),
            StmtKind::Let { value, .. } => expr(value, used),
            StmtKind::If { then, els, .. } => {
                collect_calls(then, used);
                if let Some(e) = els {
                    collect_calls(e, used);
                }
            }
            StmtKind::For { body, .. } | StmtKind::Block(body) => collect_calls(body, used),
            StmtKind::Match { arms, .. } => {
                for (_, st) in arms {
                    collect_calls(std::slice::from_ref(st), used);
                }
            }
            _ => {}
        }
    }
}

fn define_enum<'a>(d: &mut Design<'a>, i: usize, e: &'a ast::EnumDef) -> R<()> {
    let mut cx = ModCx::global();
    let mut variants: Vec<(String, BigInt)> = Vec::new();
    let mut seen: HashMap<&str, Span> = HashMap::new();
    let explicit = e.repr.is_some();
    let repr = match &e.repr {
        Some(t) => {
            let ty = cx.eval_type(d, t)?;
            match ty {
                Ty::Int(it)
                    if !it.signed()
                        && it.full_decl().is_some_and(|d| matches!(d, IntDecl::Uint(_))) =>
                {
                    Some(it)
                }
                _ => {
                    return d.err(
                        Diagnostic::error(t.span, "an enum encoding must be a `uint<N>`")
                            .with_label("not a uint"),
                    );
                }
            }
        }
        None => None,
    };
    if e.variants.is_empty() {
        return d.err(
            Diagnostic::error(e.name.span, "an enum needs at least one variant")
                .with_label("empty enum"),
        );
    }
    for (k, (v, val)) in e.variants.iter().enumerate() {
        if let Some(prev) = seen.get(v.name.as_str()) {
            d.emit(
                Diagnostic::error(v.span, format!("variant `{}` is listed twice", v.name))
                    .with_label("duplicate")
                    .with_secondary(*prev, "first listed here"),
            );
            continue;
        }
        seen.insert(&v.name, v.span);
        let code = match (&repr, val) {
            (Some(it), Some(x)) => {
                let c = cx.eval_int(d, x)?;
                if !it.contains_value(&c) {
                    d.emit(
                        Diagnostic::error(
                            x.span,
                            format!(
                                "encoding {c} does not fit {}",
                                d.types.name(&Ty::Int(it.clone()))
                            ),
                        )
                        .with_label("too large"),
                    );
                }
                if let Some((other, _)) = variants.iter().find(|(_, oc)| *oc == c) {
                    d.emit(
                        Diagnostic::error(
                            x.span,
                            format!(
                                "variants `{other}` and `{}` have the same encoding {c}",
                                v.name
                            ),
                        )
                        .with_label("duplicate encoding"),
                    );
                }
                c
            }
            (Some(_), None) => {
                return d.err(
                    Diagnostic::error(v.span, format!("variant `{}` needs an encoding", v.name))
                        .with_label("missing `= value`")
                        .with_help("an enum with `: uint<N>` gives every variant a value"),
                );
            }
            (None, Some(x)) => {
                return d.err(
                    Diagnostic::error(
                        x.span,
                        "only enums with an explicit encoding give variants values",
                    )
                    .with_label("unexpected value")
                    .with_help(format!(
                        "declare the encoding: `enum {}: uint<N> {{ ... }}`",
                        e.name.name
                    )),
                );
            }
            (None, None) => BigInt::from_u64(k as u64),
        };
        variants.push((v.name.clone(), code));
    }
    let width = match &repr {
        Some(it) => it.width(),
        None => (64 - ((variants.len() as u64).saturating_sub(1)).leading_zeros()).max(1),
    };
    let info = &mut d.types.enums[i];
    info.variants = variants;
    info.width = width;
    info.explicit = explicit;
    Ok(())
}

fn define_struct<'a>(
    d: &mut Design<'a>,
    structs: &[&'a ast::StructDef],
    i: usize,
    done: &mut [bool],
    stack: &mut Vec<usize>,
) -> R<()> {
    if done[i] {
        return Ok(());
    }
    if stack.contains(&i) {
        let s = structs[i];
        return d.err(
            Diagnostic::error(s.name.span, format!("struct `{}` contains itself", s.name.name))
                .with_label("infinitely large type"),
        );
    }
    stack.push(i);
    let s = structs[i];
    let mut fields = Vec::new();
    let mut seen: HashMap<&str, Span> = HashMap::new();
    for (f, t) in &s.fields {
        if let Some(prev) = seen.get(f.name.as_str()) {
            d.emit(
                Diagnostic::error(f.span, format!("field `{}` is declared twice", f.name))
                    .with_label("duplicate")
                    .with_secondary(*prev, "first declared here"),
            );
            continue;
        }
        seen.insert(&f.name, f.span);
        // Dependencies first.
        if let TypeKind::Named(n) = &t.kind
            && let Some(Ty::Struct(j)) = d.type_names.get(&n.name).cloned()
        {
            define_struct(d, structs, j, done, stack)?;
        }
        let ty = ModCx::global().eval_type(d, t)?;
        fields.push((f.name.clone(), ty));
    }
    if fields.is_empty() {
        d.emit(
            Diagnostic::error(s.name.span, "a struct needs at least one field")
                .with_label("empty struct"),
        );
    }
    d.types.structs[i].fields = fields;
    done[i] = true;
    stack.pop();
    Ok(())
}

fn check_global_asserts(d: &mut Design<'_>) {
    let asserts = d.asserts.clone();
    for a in asserts {
        let mut cx = ModCx::global();
        let _ = cx.check_assert(d, a);
    }
}

/// Parses a `-P` value: an integer or a double-quoted string.
fn parse_param(v: &str) -> Option<Value> {
    if let Some(s) = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Some(Value::Str(s.to_string()));
    }
    if v == "true" || v == "false" {
        return Some(Value::Hw(Hw {
            sig: Sig::bit(if v == "true" { SigBit::ONE } else { SigBit::ZERO }),
            ty: Ty::Bool,
            tags: Tags::none(),
        }));
    }
    let (digits, radix) = if let Some(h) = v.strip_prefix("0x") {
        (h, 16)
    } else if let Some(b) = v.strip_prefix("0b") {
        (b, 2)
    } else {
        (v.trim_start_matches('-'), 10)
    };
    let n = BigInt::parse(digits, radix)?;
    Some(Value::Int(if v.starts_with('-') { -&n } else { n }, None))
}

fn elaborate_top<'a>(d: &mut Design<'a>, def: &'a ast::Module, opts: &Options) -> R<()> {
    let mut given = HashMap::new();
    for (k, v) in &opts.params {
        let Some(g) = def.generics.iter().find(|g| &g.name.name == k) else {
            let mut dg = Diagnostic::global_error(format!(
                "top module `{}` has no generic parameter `{k}`",
                def.name.name
            ));
            if let Some(s) = suggest(k, def.generics.iter().map(|g| g.name.name.as_str())) {
                dg = dg.with_help(format!("did you mean `{s}`?"));
            }
            return d.err(dg);
        };
        let Some(val) = parse_param(v) else {
            return d.err(Diagnostic::global_error(format!(
                "bad value `{v}` for `{k}`: expected an integer, `true`, `false` or a \"string\""
            )));
        };
        given.insert(g.name.name.clone(), val);
    }
    let mut cx = ModCx::new(def, String::new(), true);
    cx.bind_generics(d, def, &[], given, def.name.span)?;
    d.elaborated.insert(def.name.name.clone());
    let ports = cx.port_types(d)?;
    let mut sigs = Vec::new();
    let mut pos = 0u32;
    for p in &ports {
        match &p.kind {
            PortInfoKind::Data { dir, ty, .. } => {
                if !matches!(ty, Ty::Bool | Ty::Int(_))
                    || matches!(ty, Ty::Int(i) if matches!(i.decl, Some(IntDecl::Index(_))))
                {
                    d.emit(
                        Diagnostic::error(p.span, format!("top-level port `{}` has type {}", p.name, d.types.name(ty)))
                            .with_label("not a pin type")
                            .with_help("top-level ports are `bool`, `uint<N>`, `sint<N>`, `clock` or `inout`: pack other types with `.bits()`"),
                    );
                }
                let w = d.types.width(ty);
                let s = d.add_wire(&p.name, w, p.span, true);
                let SigBit::Wire(wid, _) = s.0[0] else { unreachable!() };
                d.out.wires[wid].port = Some((*dir, pos));
                d.out.wires[wid].signed = matches!(ty, Ty::Int(i) if i.signed());
                pos += 1;
                sigs.push(PortSig::Data(s));
            }
            PortInfoKind::Clock => {
                let s = d.add_wire(&p.name, 1, p.span, true);
                let SigBit::Wire(wid, _) = s.0[0] else { unreachable!() };
                d.out.wires[wid].port = Some((PortDir::Input, pos));
                pos += 1;
                sigs.push(PortSig::Clock(s.0[0]));
            }
            PortInfoKind::ClockOut => unreachable!(),
            PortInfoKind::Domain => {
                return d.err(
                    Diagnostic::error(p.span, "the top module cannot have domain parameters")
                        .with_label("no parent provides this domain")
                        .with_help("declare a clock input (`in clk: clock`) and define domains in the body"),
                );
            }
        }
    }
    cx.bind_ports(d, &ports, sigs, HashMap::new())?;
    cx.run(d)
}

/// Checks a module that the design does not use, with placeholder ports.
fn check_standalone<'a>(d: &mut Design<'a>, def: &'a ast::Module) -> R<()> {
    let mut cx = ModCx::new(def, format!("{}.", def.name.name), false);
    cx.bind_generics(d, def, &[], HashMap::new(), def.name.span)?;
    let ports = cx.port_types(d)?;
    let mut sigs = Vec::new();
    let mut doms = HashMap::new();
    for p in &ports {
        match &p.kind {
            PortInfoKind::Data { ty, .. } => {
                let w = d.types.width(ty);
                sigs.push(PortSig::Data(d.add_wire(&p.name, w, p.span, false)));
            }
            PortInfoKind::Clock | PortInfoKind::ClockOut => {
                sigs.push(PortSig::Clock(d.add_wire(&p.name, 1, p.span, false).0[0]))
            }
            PortInfoKind::Domain => {
                let clk = d.add_wire(&p.name, 1, p.span, false).0[0];
                doms.insert(p.name.clone(), DomBinding { clk, posedge: true, reset: None });
                sigs.push(PortSig::None);
            }
        }
    }
    if cx.implicit_dom_needed() {
        let clk = d.add_wire("clk", 1, def.name.span, false).0[0];
        doms.insert("default".into(), DomBinding { clk, posedge: true, reset: None });
    }
    cx.bind_ports(d, &ports, sigs, doms)?;
    cx.run(d)
}

#[derive(Clone, Debug)]
pub(crate) enum PortDomain {
    Default,
    Named(String),
    Async,
}

#[derive(Clone, Debug)]
pub(crate) enum PortInfoKind {
    Data {
        dir: PortDir,
        ty: Ty,
        dom: PortDomain,
        default: Option<Sig>,
    },
    Clock,
    /// A clock output (extern modules only).
    ClockOut,
    Domain,
}

#[derive(Clone, Debug)]
pub(crate) struct PortInfo {
    pub name: String,
    pub span: Span,
    pub kind: PortInfoKind,
}

#[derive(Clone, Debug)]
pub(crate) enum PortSig {
    Data(Sig),
    Clock(SigBit),
    None,
}

impl<'a> ModCx<'a> {
    pub fn new(def: &'a ast::Module, prefix: String, is_top: bool) -> ModCx<'a> {
        ModCx {
            def_name: def.name.name.clone(),
            body: def.body.as_deref().unwrap_or(&[]),
            ports: &def.ports,
            mode: Mode::Module,
            prefix,
            generics: HashMap::new(),
            is_top,
            scopes: vec![Scope::default()],
            lazy: HashMap::new(),
            signals: Vec::new(),
            drivers: Vec::new(),
            domains: Vec::new(),
            implicit_dom: None,
            default_dom: None,
            default_name: None,
            insts: Vec::new(),
            mems: Vec::new(),
            sinks: Vec::new(),
            var_sources: HashMap::new(),
            partial: Vec::new(),
            loop_names: Vec::new(),
            allow_module_call: false,
            let_names: Vec::new(),
            lets_read: HashMap::new(),
            uniq: 0,
            file: def.name.span.file,
            is_extern: def.body.is_none(),
            reg_read: false,
            call_name: None,
            top_defs: Default::default(),
            dead_warned: HashSet::new(),
            connected: HashSet::new(),
            conn_defs: Default::default(),
            let_used: HashSet::new(),
            generic_decls: HashMap::new(),
            walk_failed: false,
        }
    }

    /// A context for evaluating constants outside any module.
    pub fn global() -> ModCx<'a> {
        ModCx {
            def_name: String::new(),
            body: &[],
            ports: &[],
            mode: Mode::Function,
            prefix: String::new(),
            generics: HashMap::new(),
            is_top: false,
            scopes: vec![Scope::default()],
            lazy: HashMap::new(),
            signals: Vec::new(),
            drivers: Vec::new(),
            domains: Vec::new(),
            implicit_dom: None,
            default_dom: None,
            default_name: None,
            insts: Vec::new(),
            mems: Vec::new(),
            sinks: Vec::new(),
            var_sources: HashMap::new(),
            partial: Vec::new(),
            loop_names: Vec::new(),
            allow_module_call: false,
            let_names: Vec::new(),
            lets_read: HashMap::new(),
            uniq: 0,
            file: FileId(0),
            is_extern: false,
            reg_read: false,
            call_name: None,
            top_defs: Default::default(),
            dead_warned: HashSet::new(),
            connected: HashSet::new(),
            conn_defs: Default::default(),
            let_used: HashSet::new(),
            generic_decls: HashMap::new(),
            walk_failed: false,
        }
    }

    /// Evaluates generic arguments (`args` given by the parent, or `given`
    /// values) and defaults.
    pub fn bind_generics(
        &mut self,
        d: &mut Design<'a>,
        def_generics_of: &'a ast::Module,
        args: &'a [ast::GenArg],
        given: HashMap<String, Value>,
        call_span: Span,
    ) -> R<()> {
        self.bind_generic_list(
            d,
            &def_generics_of.generics,
            &def_generics_of.name.name,
            args,
            given,
            call_span,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn bind_generic_list(
        &mut self,
        d: &mut Design<'a>,
        generics: &'a [ast::Generic],
        what: &str,
        args: &'a [ast::GenArg],
        mut given: HashMap<String, Value>,
        call_span: Span,
        caller: Option<&mut ModCx<'a>>,
    ) -> R<()> {
        let mut caller = caller;
        for a in args {
            let Some(g) = generics.iter().find(|g| g.name.name == a.name.name) else {
                let mut dg = Diagnostic::error(
                    a.name.span,
                    format!("`{what}` has no generic parameter `{}`", a.name.name),
                )
                .with_label("unknown parameter");
                if let Some(s) =
                    suggest(&a.name.name, generics.iter().map(|g| g.name.name.as_str()))
                {
                    dg = dg.with_help(format!("did you mean `{s}`?"));
                } else if !generics.is_empty() {
                    let names: Vec<String> =
                        generics.iter().map(|g| format!("`{}`", g.name.name)).collect();
                    dg = dg.with_help(format!("its parameters are {}", names.join(", ")));
                }
                d.emit(dg);
                continue;
            };
            if given.contains_key(&g.name.name) {
                d.emit(
                    Diagnostic::error(a.name.span, format!("`{}` is given twice", a.name.name))
                        .with_label("duplicate argument"),
                );
                continue;
            }
            let cx: &mut ModCx<'a> = match caller.as_deref_mut() {
                Some(c) => c,
                None => self,
            };
            let v = cx.eval_generic_arg(d, g, &a.value)?;
            given.insert(g.name.name.clone(), v);
        }
        for g in generics {
            if let Some(v) = given.remove(&g.name.name) {
                let v = self.check_generic_kind(d, g, v, call_span)?;
                self.generic_decls.insert(g.name.name.clone(), g.name.span);
                self.generics.insert(g.name.name.clone(), v);
                continue;
            }
            match &g.default {
                Some(def) => {
                    let v = self.eval_generic_arg(d, g, def)?;
                    self.generic_decls.insert(g.name.name.clone(), g.name.span);
                    self.generics.insert(g.name.name.clone(), v);
                }
                None => {
                    return d.err(
                        Diagnostic::error(
                            call_span,
                            format!("missing generic argument `{}` of `{what}`", g.name.name),
                        )
                        .with_label(format!("`{}` is required", g.name.name))
                        .with_secondary(g.name.span, "declared here without a default")
                        .with_help(format!("pass it as `{what}<{}: ...>(...)`", g.name.name)),
                    );
                }
            }
        }
        Ok(())
    }

    fn check_generic_kind(
        &mut self,
        d: &mut Design<'a>,
        g: &'a ast::Generic,
        v: Value,
        span: Span,
    ) -> R<Value> {
        let ok = match (g.kind, &v) {
            (GenericKind::Int, Value::Int(..))
            | (GenericKind::Str, Value::Str(_))
            | (GenericKind::Type, Value::Type(_)) => true,
            (GenericKind::Bool, Value::Hw(h)) => h.ty == Ty::Bool && h.sig.is_const(),
            _ => false,
        };
        if ok {
            Ok(v)
        } else {
            let kind = match g.kind {
                GenericKind::Int => "an integer",
                GenericKind::Bool => "`true` or `false`",
                GenericKind::Str => "a string",
                GenericKind::Type => "a type",
            };
            d.err(
                Diagnostic::error(
                    span,
                    format!("generic parameter `{}` expects {kind}", g.name.name),
                )
                .with_label("wrong kind of argument"),
            )
        }
    }

    fn eval_generic_arg(
        &mut self,
        d: &mut Design<'a>,
        g: &'a ast::Generic,
        v: &'a GenArgValue,
    ) -> R<Value> {
        let span = match v {
            GenArgValue::Type(t) => t.span,
            GenArgValue::Expr(e) => e.span,
        };
        let val = match (g.kind, v) {
            (GenericKind::Type, GenArgValue::Type(t)) => Value::Type(self.eval_type(d, t)?),
            (GenericKind::Type, GenArgValue::Expr(e)) => {
                return d.err(
                    Diagnostic::error(e.span, format!("`{}` expects a type", g.name.name))
                        .with_label("not a type"),
                );
            }
            (_, GenArgValue::Type(t)) => match &t.kind {
                TypeKind::Named(n) => match self.lookup(d, n)? {
                    Some(Sym::Value(v)) => v,
                    _ => {
                        return d.err(
                            Diagnostic::error(n.span, format!("`{}` is not a constant", n.name))
                                .with_label("expected a constant value"),
                        );
                    }
                },
                _ => {
                    return d.err(
                        Diagnostic::error(
                            t.span,
                            format!("`{}` expects a value, not a type", g.name.name),
                        )
                        .with_label("a type"),
                    );
                }
            },
            (_, GenArgValue::Expr(e)) => self.eval_const(d, e)?,
        };
        self.check_generic_kind(d, g, val, span)
    }

    /// Whether the module has no clock port and no domain parameter.
    pub fn implicit_dom_needed(&self) -> bool {
        !self.ports.iter().any(|p| {
            matches!(p.kind, PortKind::Domain { .. } | PortKind::Binding { .. })
                || matches!(&p.kind, PortKind::In { ty, .. } if matches!(ty.kind, TypeKind::Clock))
        })
    }

    /// Port types and domains, evaluated with the generic arguments.
    pub fn port_types(&mut self, d: &mut Design<'a>) -> R<Vec<PortInfo>> {
        let mut out = Vec::new();
        let mut seen: HashMap<&str, Span> = HashMap::new();
        let mut ok = true;
        for p in self.ports {
            if let Some(prev) = seen.get(p.name.name.as_str())
                && !matches!(p.kind, PortKind::Binding { .. })
            {
                d.emit(
                    Diagnostic::error(
                        p.name.span,
                        format!("port `{}` is declared twice", p.name.name),
                    )
                    .with_label("duplicate")
                    .with_secondary(*prev, "first declared here"),
                );
                ok = false;
                continue;
            }
            seen.insert(&p.name.name, p.name.span);
            let dom_of = |dom: &Option<ast::DomRef>| match dom {
                None => PortDomain::Default,
                Some(r) => match &r.kind {
                    ast::DomRefKind::Async => PortDomain::Async,
                    ast::DomRefKind::Default => PortDomain::Default,
                    ast::DomRefKind::Named(n) => PortDomain::Named(n.name.clone()),
                },
            };
            let kind = match &p.kind {
                PortKind::In { ty, dom, default } => {
                    if matches!(ty.kind, TypeKind::Clock) {
                        if dom.is_some() || default.is_some() {
                            d.emit(
                                Diagnostic::error(
                                    p.span,
                                    "a clock input takes no domain or default",
                                )
                                .with_label("clock port"),
                            );
                        }
                        PortInfoKind::Clock
                    } else {
                        let Ok(t) = self.eval_type(d, ty) else {
                            ok = false;
                            continue;
                        };
                        let def = match default {
                            Some(e) => match self.eval(d, e, Some(&t)).and_then(|v| {
                                self.coerce(d, v, &t, e.span, "the default value", Some(e))
                            }) {
                                Ok(h) if h.sig.is_const() => Some(h.sig),
                                Ok(_) => {
                                    d.emit(
                                        Diagnostic::error(
                                            e.span,
                                            "a port default must be a constant",
                                        )
                                        .with_label("not constant"),
                                    );
                                    None
                                }
                                Err(()) => None,
                            },
                            None => None,
                        };
                        PortInfoKind::Data {
                            dir: PortDir::Input,
                            ty: t,
                            dom: dom_of(dom),
                            default: def,
                        }
                    }
                }
                PortKind::Out { ty, dom } => {
                    if matches!(ty.kind, TypeKind::Clock)
                        && self.body.is_empty()
                        && !self.ports.is_empty()
                        && self.is_extern
                    {
                        out.push(PortInfo {
                            name: p.name.name.clone(),
                            span: p.span,
                            kind: PortInfoKind::ClockOut,
                        });
                        let _ = dom;
                        continue;
                    }
                    if matches!(ty.kind, TypeKind::Clock) {
                        d.emit(
                            Diagnostic::error(ty.span, "clock outputs are not supported")
                                .with_label("clock output")
                                .with_help(
                                    "pass the clock source to the modules that need it instead",
                                ),
                        );
                        ok = false;
                        continue;
                    }
                    let Ok(t) = self.eval_type(d, ty) else {
                        ok = false;
                        continue;
                    };
                    PortInfoKind::Data {
                        dir: PortDir::Output,
                        ty: t,
                        dom: dom_of(dom),
                        default: None,
                    }
                }
                PortKind::Inout { ty } => {
                    let Ok(t) = self.eval_type(d, ty) else {
                        ok = false;
                        continue;
                    };
                    if !self.is_top
                        && self.body.is_empty()
                        && !d.prim_files.contains(&self.file)
                        && d.verilog.is_none()
                    {
                        // Extern modules may have inout ports; checked at connection.
                    }
                    PortInfoKind::Data {
                        dir: PortDir::Inout,
                        ty: t,
                        dom: PortDomain::Async,
                        default: None,
                    }
                }
                PortKind::Domain { .. } | PortKind::Binding { .. } => PortInfoKind::Domain,
            };
            out.push(PortInfo { name: p.name.name.clone(), span: p.span, kind });
        }
        if ok { Ok(out) } else { Err(()) }
    }

    /// Binds ports to signals and domain parameters to the parent's clocks,
    /// and registers the module-level declarations.
    pub fn bind_ports(
        &mut self,
        d: &mut Design<'a>,
        ports: &[PortInfo],
        sigs: Vec<PortSig>,
        mut doms: HashMap<String, DomBinding>,
    ) -> R<()> {
        // Domains first: clock ports, domain parameters, the implicit one.
        let mut clock_port_doms = HashMap::new();
        for (p, s) in ports.iter().zip(&sigs) {
            match (&p.kind, s) {
                (PortInfoKind::Clock, PortSig::Clock(bit)) => {
                    let id = d.fresh_clock(format!("{}{}", self.prefix, p.name));
                    let dom = self.domains.len();
                    self.domains.push(DomainInfo {
                        name: p.name.clone(),
                        clock: ClockVal {
                            id,
                            bit: *bit,
                            origin: ClockOrigin::Port(p.name.clone()),
                        },
                        posedge: true,
                        reset: None,
                    });
                    clock_port_doms.insert(p.name.clone(), dom);
                    self.scopes[0].syms.insert(p.name.clone(), Sym::ClockPort(dom));
                }
                (PortInfoKind::Domain, _) => {
                    let b = doms.remove(&p.name).unwrap_or(DomBinding {
                        clk: SigBit::ZERO,
                        posedge: true,
                        reset: None,
                    });
                    let id = d.fresh_clock(format!("{}{}", self.prefix, p.name));
                    let dom = self.domains.len();
                    self.domains.push(DomainInfo {
                        name: p.name.clone(),
                        clock: ClockVal {
                            id,
                            bit: b.clk,
                            origin: ClockOrigin::Param(p.name.clone()),
                        },
                        posedge: b.posedge,
                        reset: b.reset,
                    });
                    self.scopes[0].syms.insert(p.name.clone(), Sym::Domain(dom));
                    let default_marked = self.ports.iter().any(|q| {
                        q.name.name == p.name
                            && matches!(q.kind, PortKind::Domain { default: true })
                    });
                    if default_marked {
                        self.default_name = Some(p.name.clone());
                    }
                }
                _ => {}
            }
        }
        if self.implicit_dom_needed() {
            let b = doms.remove("default");
            let id = d.fresh_clock(if self.prefix.is_empty() {
                "default".to_string()
            } else {
                format!("{}default", self.prefix)
            });
            let dom = self.domains.len();
            let (bit, posedge, reset) = match b {
                Some(b) => (b.clk, b.posedge, b.reset),
                None => {
                    d.clockless.insert(id);
                    (SigBit::ZERO, true, None)
                }
            };
            self.domains.push(DomainInfo {
                name: "default".into(),
                clock: ClockVal { id, bit, origin: ClockOrigin::Param("default".into()) },
                posedge,
                reset,
            });
            self.implicit_dom = Some(dom);
            self.default_dom = Some(dom);
        }
        // Body declarations.
        let mut body_doms = Vec::new();
        for st in self.body {
            let name = match &st.kind {
                StmtKind::Const(c) => &c.name,
                StmtKind::Let { name, .. } => {
                    self.let_names.push(name.name.clone());
                    name
                }
                StmtKind::Wire { name, .. }
                | StmtKind::Reg { name, .. }
                | StmtKind::Mem { name, .. }
                | StmtKind::Inst { name, .. } => name,
                StmtKind::Domain { name, default, .. } => {
                    body_doms.push(name.name.clone());
                    if *default {
                        if self.implicit_dom.is_some() {
                            d.emit(
                                Diagnostic::error(name.span, format!("`{}` cannot be the default domain here", name.name))
                                    .with_label("`default` not allowed")
                                    .with_help("this module has no clock input or domain parameter, so its default domain is the one its parent provides; remove `default`, and annotate declarations in this domain with `@` instead"),
                            );
                        } else if let Some(prev) = &self.default_name {
                            d.emit(
                                Diagnostic::error(
                                    name.span,
                                    format!("two default domains: `{prev}` and `{}`", name.name),
                                )
                                .with_label("second `default`"),
                            );
                        } else {
                            self.default_name = Some(name.name.clone());
                        }
                    }
                    name
                }
                _ => continue,
            };
            if self.scopes[0].syms.contains_key(&name.name) || self.lazy.contains_key(&name.name) {
                let prev = self.decl_span(&name.name).unwrap_or(name.span);
                d.emit(
                    Diagnostic::error(
                        name.span,
                        format!("`{}` is already declared in this module", name.name),
                    )
                    .with_label("second declaration")
                    .with_secondary(prev, "first declared here"),
                );
                continue;
            }
            self.lazy.insert(name.name.clone(), Lazy::Pending(st));
        }
        // Default domain: the only one, or the marked one.
        if self.default_dom.is_none() && self.default_name.is_none() {
            let total = self.domains.len() + body_doms.len();
            if total == 1 {
                self.default_name = Some(if let Some(n) = body_doms.first() {
                    n.clone()
                } else {
                    self.domains[0].name.clone()
                });
            }
        }
        if self.default_dom.is_none()
            && let Some(n) = self.default_name.clone()
            && let Some(i) = self.domains.iter().position(|x| x.name == n)
        {
            self.default_dom = Some(i);
        }
        // Data ports.
        for (p, s) in ports.iter().zip(sigs) {
            let (PortInfoKind::Data { dir, ty, dom, .. }, PortSig::Data(sig)) = (&p.kind, s) else {
                continue;
            };
            if self.lazy.contains_key(&p.name) {
                let decl = self.body.iter().find_map(|st| match &st.kind {
                    StmtKind::Const(c) if c.name.name == p.name => Some(c.name.span),
                    StmtKind::Let { name, .. }
                    | StmtKind::Wire { name, .. }
                    | StmtKind::Reg { name, .. }
                    | StmtKind::Mem { name, .. }
                    | StmtKind::Inst { name, .. }
                    | StmtKind::Domain { name, .. }
                        if name.name == p.name =>
                    {
                        Some(name.span)
                    }
                    _ => None,
                });
                d.emit(
                    Diagnostic::error(
                        decl.unwrap_or(p.span),
                        format!("`{}` is already declared as a port", p.name),
                    )
                    .with_label("second declaration")
                    .with_secondary(p.span, "port declared here")
                    .with_help("Weft has no shadowing: pick a different name"),
                );
            }
            let tag = self.port_tag(d, p, *dir, dom)?;
            let kind = match dir {
                PortDir::Input | PortDir::Inout => SigKind::In,
                PortDir::Output => SigKind::Out,
            };
            let idx = self.signals.len();
            self.signals.push(Signal {
                name: p.name.clone(),
                span: p.span,
                kind,
                ty: ty.clone(),
                sig,
                tag: Some(tag),
                read: false,
                assigned: false,
            });
            self.scopes[0].syms.insert(p.name.clone(), Sym::Signal(idx));
        }
        Ok(())
    }

    fn port_tag(
        &mut self,
        d: &mut Design<'a>,
        p: &PortInfo,
        dir: PortDir,
        dom: &PortDomain,
    ) -> R<FixedTag> {
        let top_input = self.is_top && dir == PortDir::Input;
        match dom {
            PortDomain::Async => {
                Ok(FixedTag { atom: Some(TagAtom::Async), what: "asynchronous".into() })
            }
            PortDomain::Default if top_input => Ok(FixedTag {
                atom: Some(TagAtom::Async),
                what: "an asynchronous top-level input".into(),
            }),
            PortDomain::Default if dir == PortDir::Inout => {
                Ok(FixedTag { atom: Some(TagAtom::Async), what: "an inout pin".into() })
            }
            PortDomain::Default => match self.default_domain(d, p.span)? {
                Some(i) => Ok(self.dom_tag(i)),
                None if self.is_top => {
                    Ok(FixedTag { atom: Some(TagAtom::Async), what: "asynchronous".into() })
                }
                None => d.err(self.no_default_error(d, p.span, &format!("port `{}`", p.name))),
            },
            PortDomain::Named(n) => match self.domain_by_name(d, n, p.span)? {
                Some(i) => Ok(self.dom_tag(i)),
                None => Err(()),
            },
        }
    }

    pub fn dom_tag(&self, i: usize) -> FixedTag {
        let dm = &self.domains[i];
        if dm.clock.bit == SigBit::ZERO && self.is_top {
            return FixedTag { atom: Some(TagAtom::Async), what: "asynchronous".into() };
        }
        FixedTag {
            atom: Some(TagAtom::Clock(dm.clock.id)),
            what: format!("in domain `{}`", dm.name),
        }
    }

    pub fn no_default_error(&self, d: &Design<'a>, span: Span, what: &str) -> Diagnostic {
        let names: Vec<String> =
            self.all_domain_names(d).into_iter().map(|n| format!("`@{n}`")).collect();
        Diagnostic::error(
            span,
            format!("{what} needs a domain: this module has several and none is the default"),
        )
        .with_label("no domain given")
        .with_help(format!("annotate it with one of {}", names.join(", ")))
        .with_help("or mark one domain `default`")
    }

    fn all_domain_names(&self, _d: &Design<'a>) -> Vec<String> {
        let mut v: Vec<String> = self.domains.iter().map(|x| x.name.clone()).collect();
        for st in self.body {
            if let StmtKind::Domain { name, .. } = &st.kind
                && !v.contains(&name.name)
            {
                v.push(name.name.clone());
            }
        }
        v
    }

    /// The default domain, resolving a body definition if needed.
    pub fn default_domain(&mut self, d: &mut Design<'a>, span: Span) -> R<Option<usize>> {
        if let Some(i) = self.default_dom {
            return Ok(Some(i));
        }
        let Some(n) = self.default_name.clone() else { return Ok(None) };
        let i = self.domain_by_name(d, &n, span)?;
        self.default_dom = i;
        Ok(i)
    }

    /// A domain by name: a body domain, a domain parameter or a clock port.
    pub fn domain_by_name(&mut self, d: &mut Design<'a>, n: &str, span: Span) -> R<Option<usize>> {
        let id = ast::Ident { name: n.to_string(), span };
        match self.lookup(d, &id)? {
            Some(Sym::Domain(i)) | Some(Sym::ClockPort(i)) => Ok(Some(i)),
            Some(_) => d.err(
                Diagnostic::error(span, format!("`{n}` is not a domain"))
                    .with_label("expected a domain name"),
            ),
            None => {
                let names = self.all_domain_names(d);
                let mut dg = Diagnostic::error(span, format!("unknown domain `{n}`"))
                    .with_label("not a domain of this module");
                if let Some(s) = suggest(n, names.iter().map(|s| s.as_str())) {
                    dg = dg.with_help(format!("did you mean `@{s}`?"));
                }
                d.err(dg)
            }
        }
    }

    fn decl_span(&self, name: &str) -> Option<Span> {
        for st in self.body {
            match &st.kind {
                StmtKind::Const(c) if c.name.name == name => return Some(c.name.span),
                StmtKind::Let { name: n, .. }
                | StmtKind::Wire { name: n, .. }
                | StmtKind::Reg { name: n, .. }
                | StmtKind::Mem { name: n, .. }
                | StmtKind::Inst { name: n, .. }
                | StmtKind::Domain { name: n, .. }
                    if n.name == name =>
                {
                    return Some(n.span);
                }
                _ => {}
            }
        }
        self.ports.iter().find(|p| p.name.name == name).map(|p| p.name.span)
    }

    /// Elaborates the body and finishes the module.
    pub fn run(&mut self, d: &mut Design<'a>) -> R<()> {
        let body = self.body;
        let errors = d.diags.error_count();
        let (first_cell, first_wire) = (d.out.cells.len(), d.out.wires.len());
        let ok = self.walk_module(d, body).is_ok();
        // Resolve whatever the walk did not reach (only after errors).
        let names: Vec<String> = self.lazy.keys().cloned().collect();
        for n in names {
            if matches!(self.lazy.get(&n), Some(Lazy::Pending(_))) {
                let _ = self.resolve_lazy(d, &n);
            }
        }
        self.walk_failed = !ok;
        if d.indexing {
            self.index_decls(d);
        }
        let fin = self.finish(d, errors);
        // Anonymous cells and wires carry the instance path, like named ones.
        if !self.prefix.is_empty() {
            let prefix = &self.prefix;
            for c in &mut d.out.cells.as_mut_slice()[first_cell..] {
                if c.name.starts_with('$') {
                    c.name.insert_str(0, prefix);
                }
            }
            for w in &mut d.out.wires.as_mut_slice()[first_wire..] {
                if w.name.starts_with('$') {
                    w.name.insert_str(0, prefix);
                }
            }
        }
        if ok && fin.is_ok() { Ok(()) } else { Err(()) }
    }

    pub fn name_of(&self, n: &str) -> String {
        let mut s = format!("{}{}", self.prefix, n);
        for l in &self.loop_names {
            s.push_str(l);
        }
        s
    }
}
