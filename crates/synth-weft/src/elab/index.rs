//! Source index for editors: what each identifier refers to.

use synth_core::Span;

use super::*;

/// A use (or declaration) of a name, its declaration, and a description.
#[derive(Clone, Debug)]
pub struct Reference {
    pub span: Span,
    pub decl: Option<Span>,
    pub kind: RefKind,
    /// Markdown.
    pub hover: String,
    /// Names reachable with `.` (instance ports, struct fields, variants).
    pub members: Vec<String>,
}

/// What a name refers to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RefKind {
    Module,
    Function,
    Type,
    Variant,
    Field,
    Port,
    Wire,
    Register,
    Memory,
    Instance,
    Domain,
    Const,
    Value,
    Variable,
    Generic,
}

impl<'a> Design<'a> {
    /// Records a reference (the first one recorded for a span wins).
    pub fn note(&mut self, span: Span, decl: Option<Span>, kind: RefKind, hover: String) {
        if !self.indexing || self.quiet_files.contains(&span.file) || !self.noted.insert(span) {
            return;
        }
        let decl = decl.filter(|s| !self.quiet_files.contains(&s.file));
        self.refs.push(Reference { span, decl, kind, hover, members: Vec::new() });
    }

    /// Sets the members of the reference just recorded at `span`.
    pub fn note_members(&mut self, span: Span, members: Vec<String>) {
        if let Some(r) = self.refs.last_mut()
            && r.span == span
        {
            r.members = members;
        }
    }

    /// Members of values of a type.
    pub fn type_members(&self, t: &Ty) -> Vec<String> {
        match t {
            Ty::Struct(i) => self.types.structs[*i].fields.iter().map(|(n, _)| n.clone()).collect(),
            _ => Vec::new(),
        }
    }
}

pub(crate) fn bits(n: u32) -> String {
    if n == 1 { "1 bit".into() } else { format!("{n} bits") }
}

pub(crate) fn code(s: &str) -> String {
    format!("```weft\n{s}\n```")
}

impl<'a> ModCx<'a> {
    /// The declaration of a name visible here.
    pub fn decl_of(&self, d: &Design<'a>, name: &str) -> Option<Span> {
        for s in self.scopes.iter().rev() {
            if let Some(sp) = s.decls.get(name) {
                return Some(*sp);
            }
            if s.syms.contains_key(name) {
                return self.decl_span(name);
            }
        }
        if self.lazy.contains_key(name) {
            return self.decl_span(name);
        }
        if let Some(sp) = self.generic_decls.get(name) {
            return Some(*sp);
        }
        d.consts.get(name).map(|c| c.name.span)
    }

    /// Records a use of a name that resolved to `sym`.
    pub fn note_sym(&self, d: &mut Design<'a>, id: &ast::Ident, sym: &Sym) {
        if !d.indexing {
            return;
        }
        let decl = match sym {
            Sym::Signal(i) => self.decl_of(d, &id.name).or(Some(self.signals[*i].span)),
            Sym::Inst(i) => self.decl_of(d, &id.name).or(Some(self.insts[*i].span)),
            Sym::Mem(i) => Some(self.mems[*i].span),
            _ => self.decl_of(d, &id.name),
        };
        let hover = self.describe_sym(d, &id.name, sym);
        d.note(id.span, decl, self.sym_kind(sym), hover);
        let members = self.sym_members(d, sym);
        if !members.is_empty() {
            d.note_members(id.span, members);
        }
    }

    pub fn sym_kind(&self, sym: &Sym) -> RefKind {
        match sym {
            Sym::Value(Value::Type(_)) => RefKind::Type,
            Sym::Value(Value::Hw(h)) if h.tags.is_static() && h.sig.is_const() => RefKind::Const,
            Sym::Value(Value::Int(..) | Value::Str(_)) => RefKind::Const,
            Sym::Value(_) => RefKind::Value,
            Sym::Var(_) => RefKind::Variable,
            Sym::Signal(i) => match self.signals[*i].kind {
                SigKind::Reg { .. } => RefKind::Register,
                SigKind::Wire => RefKind::Wire,
                _ => RefKind::Port,
            },
            Sym::Domain(_) | Sym::ClockPort(_) => RefKind::Domain,
            Sym::Inst(_) => RefKind::Instance,
            Sym::Mem(_) => RefKind::Memory,
        }
    }

    fn sym_members(&self, d: &Design<'a>, sym: &Sym) -> Vec<String> {
        match sym {
            Sym::Inst(i) => {
                let inst = &self.insts[*i];
                inst.ports
                    .iter()
                    .map(|(n, _)| n.clone())
                    .chain(inst.clocks.iter().map(|(n, _)| n.clone()))
                    .collect()
            }
            Sym::Signal(i) => d.type_members(&self.signals[*i].ty),
            Sym::Value(Value::Hw(h)) | Sym::Var(h) => d.type_members(&h.ty),
            Sym::Value(Value::Type(Ty::Enum(e))) => {
                d.types.enums[*e].variants.iter().map(|(n, _)| n.clone()).collect()
            }
            Sym::Domain(_) | Sym::ClockPort(_) => vec!["clock".into(), "reset".into()],
            _ => Vec::new(),
        }
    }

    /// Records the declarations of a module once its body is elaborated.
    pub fn index_decls(&self, d: &mut Design<'a>) {
        for (i, s) in self.signals.iter().enumerate() {
            if matches!(s.kind, SigKind::In | SigKind::Out | SigKind::Wire | SigKind::Reg { .. }) {
                let sp =
                    self.decl_span(&s.name).filter(|sp| sp.file == s.span.file).unwrap_or(s.span);
                d.note(sp, Some(sp), self.sym_kind(&Sym::Signal(i)), self.describe_signal(d, i));
                d.note_members(sp, d.type_members(&s.ty));
            }
        }
        for (name, l) in &self.lazy {
            if let Lazy::Done(sym) = l
                && let Some(sp) = self.decl_span(name)
            {
                d.note(sp, Some(sp), self.sym_kind(sym), self.describe_sym(d, name, sym));
                d.note_members(sp, self.sym_members(d, sym));
            }
        }
        for (name, sym) in &self.scopes.first().map(|s| s.syms.clone()).unwrap_or_default() {
            if let Some(sp) = self.decl_span(name) {
                d.note(sp, Some(sp), self.sym_kind(sym), self.describe_sym(d, name, sym));
                d.note_members(sp, self.sym_members(d, sym));
            }
        }
        for (name, sp) in &self.generic_decls {
            if let Some(v) = self.generics.get(name) {
                d.note(*sp, Some(*sp), RefKind::Generic, self.describe_value(d, name, v));
            }
        }
    }

    pub fn describe_signal(&self, d: &Design<'a>, i: usize) -> String {
        let s = &self.signals[i];
        let ty = d.types.name(&s.ty);
        let head = match &s.kind {
            SigKind::In => format!("in {}: {ty}", s.name),
            SigKind::Out => format!("out {}: {ty}", s.name),
            SigKind::Wire => format!("wire {}: {ty}", s.name),
            SigKind::Reg { has_reset, value, .. } => {
                let v = synth_core::BigInt::from_logic_vec(
                    value,
                    matches!(&s.ty, Ty::Int(t) if t.signed()),
                )
                .map(|v| v.to_string())
                .unwrap_or_else(|| format!("{value:?}"));
                if *has_reset {
                    format!("reg {}: {ty} = {v}", s.name)
                } else {
                    format!("reg {}: {ty} init {v}", s.name)
                }
            }
            SigKind::InstIn { inst, .. } | SigKind::InstOut { inst } => {
                let i = &self.insts[*inst];
                let port = s.name.strip_prefix(&format!("{}.", i.name)).unwrap_or(&s.name);
                let dir = if matches!(s.kind, SigKind::InstIn { .. }) { "in" } else { "out" };
                format!("{dir} {port}: {ty}")
            }
        };
        let mut out = code(&head);
        if let Ty::Int(t) = &s.ty {
            out.push_str(&format!("\n\nvalues {}, {}", t.range_text(), bits(t.width())));
        } else {
            out.push_str(&format!("\n\n{}", bits(d.types.width(&s.ty))));
        }
        if let Some(t) = &s.tag {
            out.push_str(&format!(" · {}", t.what));
        }
        out
    }

    pub fn describe_value(&self, d: &Design<'a>, name: &str, v: &Value) -> String {
        match v {
            Value::Int(n, _) => code(&format!("const {name}: int = {n}")),
            Value::Str(s) => code(&format!("const {name}: string = {s:?}")),
            Value::Type(t) => code(&format!("type {name} = {}", d.types.name(t))),
            Value::Hw(h) => {
                let ty = d.types.name(&h.ty);
                if h.tags.is_static() && h.sig.is_const() {
                    return match super::expr::const_int(h) {
                        Some(c) => code(&format!("const {name}: {ty} = {c}")),
                        None => code(&format!("const {name}: {ty}")),
                    };
                }
                let mut s = code(&format!("let {name}: {ty}"));
                if let Ty::Int(t) = &h.ty {
                    s.push_str(&format!("\n\nvalues {}, {}", t.range_text(), bits(t.width())));
                }
                s
            }
            _ => code(name),
        }
    }

    pub fn describe_sym(&self, d: &Design<'a>, name: &str, sym: &Sym) -> String {
        match sym {
            Sym::Value(v) => self.describe_value(d, name, v),
            Sym::Var(h) => code(&format!("var {name}: {}", d.types.name(&h.ty))),
            Sym::Signal(i) => self.describe_signal(d, *i),
            Sym::Domain(i) | Sym::ClockPort(i) => {
                let dm = &self.domains[*i];
                let reset = match dm.reset {
                    Some((_, true)) => "asynchronous reset",
                    Some((_, false)) => "synchronous reset",
                    None => "no reset",
                };
                let edge = if dm.posedge { "rising edge" } else { "falling edge" };
                let what =
                    if matches!(sym, Sym::ClockPort(_)) { "in {} : clock" } else { "domain {}" };
                code(&what.replace("{}", name)) + &format!("\n\n{edge}, {reset}")
            }
            Sym::Inst(i) => {
                let inst = &self.insts[*i];
                let ports: Vec<String> = inst
                    .ports
                    .iter()
                    .map(|(n, s)| format!("{n}: {}", d.types.name(&self.signals[*s].ty)))
                    .collect();
                code(&format!("inst {name} = {}(...)", inst.module))
                    + &format!("\n\nports: {}", ports.join(", "))
            }
            Sym::Mem(i) => {
                let m = &self.mems[*i];
                code(&format!("mem {name}: [{}; {}]", d.types.name(&m.elem), m.depth))
                    + &format!("\n\ndomain `{}`", self.domains[m.dom].name)
            }
        }
    }
}

impl<'a> Design<'a> {
    /// Source text with whitespace collapsed.
    fn src(&self, file: FileId, start: u32, end: u32) -> String {
        let s = self.sm.snippet(Span { file, start, end });
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    pub fn module_hover(&self, m: &ast::Module) -> String {
        let f = m.name.span.file;
        let kw = if m.body.is_none() { "extern module" } else { "module" };
        let Some(first) = m.ports.first() else {
            return code(&format!("{kw} {}()", m.name.name));
        };
        let head = self.src(f, m.name.span.start, first.span.start);
        let ports: Vec<String> = m
            .ports
            .iter()
            .map(|p| format!("    {},", self.src(f, p.span.start, p.span.end)))
            .collect();
        code(&format!("{kw} {head}\n{}\n)", ports.join("\n")))
    }

    pub fn fn_hover(&self, func: &ast::Function) -> String {
        code(&self.src(func.name.span.file, func.span.start, func.ret.span.end))
    }

    pub fn port_hover(&self, m: &ast::Module, port: &ast::Port) -> String {
        let f = port.span.file;
        code(&self.src(f, port.span.start, port.span.end))
            + &format!("\n\nport of `{}`", m.name.name)
    }

    /// A port of a module, by name.
    pub fn port_of(&self, module: &str, port: &str) -> Option<&'a ast::Port> {
        self.modules.get(module)?.ports.iter().find(|p| p.name.name == port)
    }

    pub fn type_decl(&self, t: &Ty) -> Option<Span> {
        match t {
            Ty::Enum(i) => self.enum_defs.get(*i).map(|e| e.name.span),
            Ty::Struct(i) => self.struct_defs.get(*i).map(|s| s.name.span),
            _ => None,
        }
    }

    pub fn type_hover(&self, t: &Ty) -> String {
        match t {
            Ty::Enum(i) => {
                let e = &self.types.enums[*i];
                let vs: Vec<String> =
                    e.variants.iter().map(|(n, c)| format!("    {n} = {c},")).collect();
                code(&format!("enum {} {{\n{}\n}}", e.name, vs.join("\n")))
                    + &format!("\n\n{}", bits(e.width))
            }
            Ty::Struct(i) => {
                let s = &self.types.structs[*i];
                let fs: Vec<String> = s
                    .fields
                    .iter()
                    .map(|(n, t)| format!("    {n}: {},", self.types.name(t)))
                    .collect();
                code(&format!("struct {} {{\n{}\n}}", s.name, fs.join("\n")))
                    + &format!("\n\n{}", bits(self.types.width(t)))
            }
            other => {
                code(&self.types.name(other)) + &format!("\n\n{}", bits(self.types.width(other)))
            }
        }
    }
}
