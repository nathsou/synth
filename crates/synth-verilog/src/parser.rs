//! Recursive-descent parser for synthesizable Verilog-2005, with
//! precedence climbing for expressions.

use synth_core::{Diagnostic, Span};

use crate::ast::*;
use crate::lexer::{Tok, Token};

/// Parse result: the syntax tree and any diagnostics.
#[derive(Debug)]
pub struct Parsed {
    pub source: SourceText,
    pub diagnostics: Vec<Diagnostic>,
}

/// Parses a preprocessed token stream (ending in [`Tok::Eof`]).
pub fn parse(tokens: &[Token]) -> Parsed {
    let mut p = Parser { toks: tokens, pos: 0, diags: Vec::new() };
    let source = p.source_text();
    Parsed { source, diagnostics: p.diags }
}

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    diags: Vec<Diagnostic>,
}

/// Marker for an error that has been reported.
#[derive(Debug)]
struct Reported;

type PResult<T> = Result<T, Reported>;

const KEYWORDS: &[&str] = &[
    "always",
    "and",
    "assign",
    "automatic",
    "begin",
    "buf",
    "bufif0",
    "bufif1",
    "case",
    "casex",
    "casez",
    "cell",
    "cmos",
    "config",
    "deassign",
    "default",
    "defparam",
    "design",
    "disable",
    "edge",
    "else",
    "end",
    "endcase",
    "endconfig",
    "endfunction",
    "endgenerate",
    "endmodule",
    "endprimitive",
    "endspecify",
    "endtable",
    "endtask",
    "event",
    "for",
    "force",
    "forever",
    "fork",
    "function",
    "generate",
    "genvar",
    "highz0",
    "highz1",
    "if",
    "ifnone",
    "incdir",
    "include",
    "initial",
    "inout",
    "input",
    "instance",
    "integer",
    "join",
    "large",
    "liblist",
    "library",
    "localparam",
    "macromodule",
    "medium",
    "module",
    "nand",
    "negedge",
    "nmos",
    "nor",
    "noshowcancelled",
    "not",
    "notif0",
    "notif1",
    "or",
    "output",
    "parameter",
    "pmos",
    "posedge",
    "primitive",
    "pull0",
    "pull1",
    "pulldown",
    "pullup",
    "pulsestyle_onevent",
    "pulsestyle_ondetect",
    "rcmos",
    "real",
    "realtime",
    "reg",
    "release",
    "repeat",
    "rnmos",
    "rpmos",
    "rtran",
    "rtranif0",
    "rtranif1",
    "scalared",
    "showcancelled",
    "signed",
    "small",
    "specify",
    "specparam",
    "strong0",
    "strong1",
    "supply0",
    "supply1",
    "table",
    "task",
    "time",
    "tran",
    "tranif0",
    "tranif1",
    "tri",
    "tri0",
    "tri1",
    "triand",
    "trior",
    "trireg",
    "unsigned",
    "use",
    "uwire",
    "vectored",
    "wait",
    "wand",
    "weak0",
    "weak1",
    "while",
    "wire",
    "wor",
    "xnor",
    "xor",
    "logic",
    "always_ff",
    "always_comb",
    "always_latch",
];

const GATES: &[&str] = &["and", "or", "nand", "nor", "xor", "xnor", "not", "buf"];

fn is_keyword(s: &str) -> bool {
    KEYWORDS.contains(&s)
}

impl<'a> Parser<'a> {
    // ---------- token helpers ----------

    fn tok(&self) -> &'a Tok {
        &self.toks[self.pos].tok
    }

    fn tok_at(&self, k: usize) -> &'a Tok {
        let i = (self.pos + k).min(self.toks.len() - 1);
        &self.toks[i].tok
    }

    fn span(&self) -> Span {
        self.toks[self.pos].span
    }

    fn prev_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1)].span
    }

    fn bump(&mut self) -> &'a Token {
        let t = &self.toks[self.pos];
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at_eof(&self) -> bool {
        *self.tok() == Tok::Eof
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(self.tok(), Tok::Op(o) if *o == op)
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.tok(), Tok::Ident(s) if s == kw)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn describe(t: &Tok) -> String {
        match t {
            Tok::Ident(s) => format!("`{s}`"),
            Tok::SysIdent(s) => format!("`${s}`"),
            Tok::Directive(s) => format!("`` `{s} ``"),
            Tok::Decimal(s) | Tok::Real(s) => format!("number `{s}`"),
            Tok::Based { .. } => "number".into(),
            Tok::Str(_) => "string".into(),
            Tok::Op(o) => format!("`{o}`"),
            Tok::AttrOpen => "`(*`".into(),
            Tok::AttrClose => "`*)`".into(),
            Tok::Newline => "end of line".into(),
            Tok::Eof => "end of file".into(),
        }
    }

    fn error<T>(&mut self, msg: impl Into<String>) -> PResult<T> {
        let msg = format!("{}, found {}", msg.into(), Self::describe(self.tok()));
        self.diags.push(Diagnostic::error(self.span(), msg));
        Err(Reported)
    }

    fn expect_op(&mut self, op: &str) -> PResult<Span> {
        if self.is_op(op) { Ok(self.bump().span) } else { self.error(format!("expected `{op}`")) }
    }

    fn ident(&mut self) -> PResult<Ident> {
        match self.tok() {
            Tok::Ident(s) if !is_keyword(s) => {
                let t = self.bump();
                Ok(Ident { name: s.clone(), span: t.span })
            }
            _ => self.error("expected an identifier"),
        }
    }

    /// Skips to just after the next `;` (or to a token that can start a new
    /// item), for error recovery.
    fn recover_item(&mut self) {
        let mut depth = 0i32;
        while !self.at_eof() {
            match self.tok() {
                Tok::Op(";") if depth <= 0 => {
                    self.bump();
                    return;
                }
                Tok::Ident(s)
                    if depth <= 0
                        && matches!(
                            s.as_str(),
                            "endmodule"
                                | "end"
                                | "endcase"
                                | "endfunction"
                                | "endtask"
                                | "endgenerate"
                        ) =>
                {
                    return;
                }
                Tok::Ident(s) if s == "begin" => depth += 1,
                Tok::Ident(s) if s == "end" => depth -= 1,
                _ => {}
            }
            self.bump();
        }
    }

    // ---------- top level ----------

    fn source_text(&mut self) -> SourceText {
        let mut modules = Vec::new();
        while !self.at_eof() {
            let attrs = self.attributes().unwrap_or_default();
            if self.is_kw("module") || self.is_kw("macromodule") {
                match self.module(attrs) {
                    Ok(m) => modules.push(m),
                    Err(Reported) => {
                        // Skip to the end of this module.
                        while !self.at_eof() && !self.is_kw("endmodule") {
                            self.bump();
                        }
                        self.eat_kw("endmodule");
                    }
                }
            } else if self.is_kw("primitive") {
                let span = self.span();
                self.diags
                    .push(Diagnostic::error(span, "user-defined primitives are not supported"));
                while !self.at_eof() && !self.is_kw("endprimitive") {
                    self.bump();
                }
                self.eat_kw("endprimitive");
            } else {
                let _ = self.error::<()>("expected `module`");
                self.bump();
            }
        }
        SourceText { modules }
    }

    fn attributes(&mut self) -> PResult<Attrs> {
        let mut attrs = Vec::new();
        while *self.tok() == Tok::AttrOpen {
            self.bump();
            loop {
                let name = self.ident_or_keyword()?;
                let value = if self.eat_op("=") { Some(self.expr()?) } else { None };
                attrs.push(Attribute { name, value });
                if !self.eat_op(",") {
                    break;
                }
            }
            if *self.tok() != Tok::AttrClose {
                return self.error("expected `*)`");
            }
            self.bump();
        }
        Ok(attrs)
    }

    /// Attribute names may be keywords (e.g. `(* full_case *)` is fine, but
    /// so is `(* parallel_case *)`).
    fn ident_or_keyword(&mut self) -> PResult<Ident> {
        match self.tok() {
            Tok::Ident(s) => {
                let t = self.bump();
                Ok(Ident { name: s.clone(), span: t.span })
            }
            _ => self.error("expected an identifier"),
        }
    }

    fn module(&mut self, attrs: Attrs) -> PResult<Module> {
        let start = self.bump().span;
        let name = self.ident()?;
        let mut params = Vec::new();
        if self.eat_op("#") {
            self.expect_op("(")?;
            if !self.is_op(")") {
                let mut cur: Option<ParamDecl> = None;
                loop {
                    if self.is_kw("parameter") || self.is_kw("localparam") {
                        if let Some(p) = cur.take() {
                            params.push(p);
                        }
                        let local = self.bump_kw_is("localparam");
                        cur = Some(self.param_header(local)?);
                    } else if cur.is_none() {
                        cur = Some(self.param_header(false)?);
                    }
                    let n = self.ident()?;
                    self.expect_op("=")?;
                    let e = self.expr()?;
                    cur.as_mut().unwrap().assigns.push((n, e));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                params.extend(cur);
            }
            self.expect_op(")")?;
        }
        let ports = if self.eat_op("(") {
            let ports = self.port_list()?;
            self.expect_op(")")?;
            ports
        } else {
            Ports::NonAnsi(vec![])
        };
        self.expect_op(";")?;
        let mut items = Vec::new();
        while !self.is_kw("endmodule") {
            if self.at_eof() {
                return self.error("expected `endmodule`");
            }
            match self.module_item() {
                Ok(Some(mut it)) => items.append(&mut it),
                Ok(None) => {}
                Err(Reported) => self.recover_item(),
            }
        }
        let end = self.bump().span;
        Ok(Module { name, attrs, params, ports, items, span: start.to(end) })
    }

    fn bump_kw_is(&mut self, kw: &str) -> bool {
        let is = self.is_kw(kw);
        self.bump();
        is
    }

    /// `[signed] [range]` or a type keyword, after `parameter`/`localparam`.
    fn param_header(&mut self, local: bool) -> PResult<ParamDecl> {
        let span = self.span();
        let mut kind = None;
        if self.is_kw("integer")
            || self.is_kw("real")
            || self.is_kw("time")
            || self.is_kw("realtime")
        {
            kind = Some(match self.bump() {
                Token { tok: Tok::Ident(s), .. } if s == "integer" => NetKind::Integer,
                Token { tok: Tok::Ident(s), .. } if s == "time" => NetKind::Time,
                _ => NetKind::Real,
            });
        }
        let signed = self.eat_kw("signed");
        self.eat_kw("unsigned");
        let range = self.opt_range()?;
        Ok(ParamDecl { local, signed, range, kind, assigns: Vec::new(), span })
    }

    fn opt_range(&mut self) -> PResult<Option<Range>> {
        if self.is_op("[") {
            let start = self.bump().span;
            let msb = self.expr()?;
            self.expect_op(":")?;
            let lsb = self.expr()?;
            let end = self.expect_op("]")?;
            Ok(Some(Range { msb, lsb, span: start.to(end) }))
        } else {
            Ok(None)
        }
    }

    fn direction(&mut self) -> Option<Direction> {
        let d = match self.tok() {
            Tok::Ident(s) if s == "input" => Direction::Input,
            Tok::Ident(s) if s == "output" => Direction::Output,
            Tok::Ident(s) if s == "inout" => Direction::Inout,
            _ => return None,
        };
        self.bump();
        Some(d)
    }

    fn net_kind(&mut self) -> Option<NetKind> {
        let k = match self.tok() {
            Tok::Ident(s) => match s.as_str() {
                "wire" | "uwire" => NetKind::Wire,
                "tri" | "tri0" | "tri1" => NetKind::Tri,
                "wand" | "triand" => NetKind::Wand,
                "wor" | "trior" => NetKind::Wor,
                "supply0" => NetKind::Supply0,
                "supply1" => NetKind::Supply1,
                "reg" => NetKind::Reg,
                "integer" => NetKind::Integer,
                "time" => NetKind::Time,
                "real" | "realtime" => NetKind::Real,
                "logic" => NetKind::Logic,
                _ => return None,
            },
            _ => return None,
        };
        self.bump();
        Some(k)
    }

    fn port_list(&mut self) -> PResult<Ports> {
        if self.is_op(")") {
            return Ok(Ports::NonAnsi(vec![]));
        }
        let ansi = *self.tok() == Tok::AttrOpen
            || matches!(self.tok(), Tok::Ident(s) if matches!(s.as_str(), "input" | "output" | "inout"));
        if !ansi {
            let mut names = Vec::new();
            loop {
                names.push(self.ident()?);
                // Port expressions like `.a(b)` are not supported.
                if !self.eat_op(",") {
                    break;
                }
            }
            return Ok(Ports::NonAnsi(names));
        }
        let mut decls: Vec<PortDecl> = Vec::new();
        loop {
            let attrs = self.attributes()?;
            let start = self.span();
            if let Some(dir) = self.direction() {
                let kind = self.net_kind();
                let signed = self.eat_kw("signed");
                self.eat_kw("unsigned");
                let range = self.opt_range()?;
                let name = self.ident()?;
                let init = if self.eat_op("=") { Some(self.expr()?) } else { None };
                decls.push(PortDecl {
                    attrs,
                    dir,
                    kind,
                    signed,
                    range,
                    names: vec![(name, init)],
                    span: start.to(self.prev_span()),
                });
            } else {
                // Continues the previous declaration: `input a, b`.
                let name = self.ident()?;
                let init = if self.eat_op("=") { Some(self.expr()?) } else { None };
                match decls.last_mut() {
                    Some(d) => d.names.push((name, init)),
                    None => return self.error("expected a port direction"),
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        Ok(Ports::Ansi(decls))
    }

    // ---------- module items ----------

    fn module_item(&mut self) -> PResult<Option<Vec<Item>>> {
        let attrs = self.attributes()?;
        let start = self.span();
        let Tok::Ident(word) = self.tok() else {
            if self.eat_op(";") {
                return Ok(None);
            }
            return self.error("expected a module item");
        };
        let item = match word.as_str() {
            "input" | "output" | "inout" => {
                let dir = self.direction().unwrap();
                let kind = self.net_kind();
                let signed = self.eat_kw("signed");
                self.eat_kw("unsigned");
                let range = self.opt_range()?;
                let mut names = Vec::new();
                loop {
                    let n = self.ident()?;
                    let init = if self.eat_op("=") { Some(self.expr()?) } else { None };
                    names.push((n, init));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op(";")?;
                Item::Port(PortDecl {
                    attrs,
                    dir,
                    kind,
                    signed,
                    range,
                    names,
                    span: start.to(self.prev_span()),
                })
            }
            "wire" | "uwire" | "tri" | "tri0" | "tri1" | "wand" | "triand" | "wor" | "trior"
            | "supply0" | "supply1" | "reg" | "integer" | "time" | "real" | "realtime"
            | "logic" => Item::Var(self.var_decl(attrs)?),
            "parameter" | "localparam" => {
                let local = self.bump_kw_is("localparam");
                let mut p = self.param_header(local)?;
                loop {
                    let n = self.ident()?;
                    self.expect_op("=")?;
                    let e = self.expr()?;
                    p.assigns.push((n, e));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op(";")?;
                p.span = start.to(self.prev_span());
                Item::Param(p)
            }
            "specparam" => {
                self.recover_item();
                return Ok(None);
            }
            "genvar" => {
                self.bump();
                let mut names = vec![self.ident()?];
                while self.eat_op(",") {
                    names.push(self.ident()?);
                }
                self.expect_op(";")?;
                Item::Genvar(names)
            }
            "assign" => {
                self.bump();
                self.skip_delay()?;
                let mut assigns = Vec::new();
                loop {
                    let lhs = self.lvalue()?;
                    self.expect_op("=")?;
                    let rhs = self.expr()?;
                    assigns.push((lhs, rhs));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op(";")?;
                Item::ContAssign { attrs, assigns, span: start.to(self.prev_span()) }
            }
            "always" | "always_ff" | "always_comb" | "always_latch" => {
                let comb = word == "always_comb";
                self.bump();
                let body = self.stmt()?;
                let body = if comb {
                    Stmt::Timing {
                        control: EventControl::Star,
                        span: body.span(),
                        body: Box::new(body),
                    }
                } else {
                    body
                };
                Item::Always { attrs, body, span: start.to(self.prev_span()) }
            }
            "initial" => {
                self.bump();
                let body = self.stmt()?;
                Item::Initial { body, span: start.to(self.prev_span()) }
            }
            "generate" => {
                self.bump();
                let mut items = Vec::new();
                while !self.is_kw("endgenerate") {
                    if self.at_eof() {
                        return self.error("expected `endgenerate`");
                    }
                    match self.module_item() {
                        Ok(Some(mut it)) => items.append(&mut it),
                        Ok(None) => {}
                        Err(Reported) => self.recover_item(),
                    }
                }
                self.bump();
                return Ok(Some(items));
            }
            "for" => self.gen_for()?,
            "if" => self.gen_if()?,
            "case" => self.gen_case()?,
            "begin" => Item::GenBlock(self.gen_block()?),
            "function" => Item::Function(self.subroutine(true)?),
            "task" => Item::Task(self.subroutine(false)?),
            "defparam" => {
                self.bump();
                let mut v = Vec::new();
                loop {
                    let mut path = vec![self.ident()?];
                    while self.eat_op(".") {
                        path.push(self.ident()?);
                    }
                    self.expect_op("=")?;
                    v.push((path, self.expr()?));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op(";")?;
                Item::Defparam(v, start.to(self.prev_span()))
            }
            "specify" => {
                while !self.at_eof() && !self.is_kw("endspecify") {
                    self.bump();
                }
                self.eat_kw("endspecify");
                return Ok(None);
            }
            "event" | "wait" | "fork" | "force" | "release" => {
                return self.error(format!("`{word}` is not supported for synthesis"));
            }
            g if GATES.contains(&g) => {
                let kind = g.to_string();
                self.bump();
                self.skip_delay()?;
                let mut items = Vec::new();
                loop {
                    let s = self.span();
                    let name = if matches!(self.tok(), Tok::Ident(_)) {
                        Some(self.ident()?)
                    } else {
                        None
                    };
                    self.expect_op("(")?;
                    let mut conns = vec![self.expr()?];
                    while self.eat_op(",") {
                        conns.push(self.expr()?);
                    }
                    self.expect_op(")")?;
                    items.push(Item::Gate(GateInstance {
                        kind: kind.clone(),
                        name,
                        conns,
                        span: s.to(self.prev_span()),
                    }));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op(";")?;
                return Ok(Some(items));
            }
            _ if !is_keyword(word) => Item::Instance(self.instance(attrs)?),
            _ => return self.error("expected a module item"),
        };
        Ok(Some(vec![item]))
    }

    fn skip_delay(&mut self) -> PResult<()> {
        if self.eat_op("#") {
            if self.eat_op("(") {
                let mut depth = 1;
                while depth > 0 && !self.at_eof() {
                    if self.is_op("(") {
                        depth += 1;
                    } else if self.is_op(")") {
                        depth -= 1;
                    }
                    self.bump();
                }
            } else {
                self.primary()?;
            }
        }
        Ok(())
    }

    fn var_decl(&mut self, attrs: Attrs) -> PResult<VarDecl> {
        let start = self.span();
        let kind = self.net_kind().unwrap();
        // `wire reg` is invalid, but `wire logic` style combinations are tolerated.
        let _ = self.net_kind();
        self.eat_kw("scalared");
        self.eat_kw("vectored");
        let signed = self.eat_kw("signed");
        self.eat_kw("unsigned");
        let range = self.opt_range()?;
        self.skip_delay()?;
        let mut names = Vec::new();
        loop {
            let name = self.ident()?;
            let mut dims = Vec::new();
            while let Some(r) = self.opt_range()? {
                dims.push(r);
            }
            let init = if self.eat_op("=") { Some(self.expr()?) } else { None };
            names.push(DeclName { name, dims, init });
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(";")?;
        Ok(VarDecl { attrs, kind, signed, range, names, span: start.to(self.prev_span()) })
    }

    fn instance(&mut self, attrs: Attrs) -> PResult<InstanceDecl> {
        let start = self.span();
        let module = self.ident()?;
        let mut params = None;
        if self.eat_op("#") {
            self.expect_op("(")?;
            if self.is_op(".") {
                let mut v = Vec::new();
                loop {
                    self.expect_op(".")?;
                    let n = self.ident()?;
                    self.expect_op("(")?;
                    let e = if self.is_op(")") { None } else { Some(self.expr()?) };
                    self.expect_op(")")?;
                    v.push((n, e));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                params = Some(ParamOverride::Named(v));
            } else if !self.is_op(")") {
                let mut v = vec![self.expr()?];
                while self.eat_op(",") {
                    v.push(self.expr()?);
                }
                params = Some(ParamOverride::Ordered(v));
            }
            self.expect_op(")")?;
        }
        let mut instances = Vec::new();
        loop {
            let s = self.span();
            let name = self.ident()?;
            let range = self.opt_range()?;
            self.expect_op("(")?;
            let mut conns = Vec::new();
            if !self.is_op(")") {
                loop {
                    if self.is_op(".") {
                        let dot = self.bump().span;
                        if self.eat_op("*") {
                            conns.push(PortConn::Wildcard(dot));
                        } else {
                            let n = self.ident()?;
                            if self.eat_op("(") {
                                let e = if self.is_op(")") { None } else { Some(self.expr()?) };
                                self.expect_op(")")?;
                                conns.push(PortConn::Named(n, e));
                            } else {
                                // `.name` shorthand (SystemVerilog): connect to same name.
                                let e = Expr::Ident(vec![(n.clone(), None)], n.span);
                                conns.push(PortConn::Named(n, Some(e)));
                            }
                        }
                    } else if self.is_op(",") || self.is_op(")") {
                        conns.push(PortConn::Ordered(None));
                    } else {
                        conns.push(PortConn::Ordered(Some(self.expr()?)));
                    }
                    if !self.eat_op(",") {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
            instances.push(Instance { name, range, conns, span: s.to(self.prev_span()) });
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(";")?;
        Ok(InstanceDecl { attrs, module, params, instances, span: start.to(self.prev_span()) })
    }

    fn gen_block(&mut self) -> PResult<GenBlock> {
        let start = self.span();
        if self.eat_kw("begin") {
            let name = if self.eat_op(":") { Some(self.ident()?) } else { None };
            let mut items = Vec::new();
            while !self.is_kw("end") {
                if self.at_eof() {
                    return self.error("expected `end`");
                }
                match self.module_item() {
                    Ok(Some(mut it)) => items.append(&mut it),
                    Ok(None) => {}
                    Err(Reported) => self.recover_item(),
                }
            }
            self.bump();
            if self.eat_op(":") {
                self.ident()?;
            }
            Ok(GenBlock { name, items, span: start.to(self.prev_span()) })
        } else {
            let items = self.module_item()?.unwrap_or_default();
            Ok(GenBlock { name: None, items, span: start.to(self.prev_span()) })
        }
    }

    fn gen_for(&mut self) -> PResult<Item> {
        let start = self.bump().span;
        self.expect_op("(")?;
        self.eat_kw("genvar");
        let v = self.ident()?;
        self.expect_op("=")?;
        let init = self.expr()?;
        self.expect_op(";")?;
        let cond = self.expr()?;
        self.expect_op(";")?;
        let sv = self.ident()?;
        let step = self.step_expr(&sv)?;
        self.expect_op(")")?;
        let body = self.gen_block()?;
        Ok(Item::GenFor {
            init: (v, init),
            cond,
            step: (sv, step),
            body,
            span: start.to(self.prev_span()),
        })
    }

    /// The right-hand side of a loop step: `i = expr`, `i++`, `i += expr`.
    fn step_expr(&mut self, var: &Ident) -> PResult<Expr> {
        let target = Expr::Ident(vec![(var.clone(), None)], var.span);
        let one = Expr::Number(
            Number { size: None, signed: true, base: None, digits: "1".into() },
            var.span,
        );
        let bin = |op, rhs: Expr| Expr::Binary {
            op,
            lhs: Box::new(target.clone()),
            rhs: Box::new(rhs),
            span: var.span,
        };
        if self.eat_op("=") {
            self.expr()
        } else if self.eat_op("++") {
            Ok(bin(BinaryOp::Add, one))
        } else if self.eat_op("--") {
            Ok(bin(BinaryOp::Sub, one))
        } else if self.eat_op("+=") {
            let e = self.expr()?;
            Ok(bin(BinaryOp::Add, e))
        } else if self.eat_op("-=") {
            let e = self.expr()?;
            Ok(bin(BinaryOp::Sub, e))
        } else {
            self.error("expected a loop step assignment")
        }
    }

    fn gen_if(&mut self) -> PResult<Item> {
        let start = self.bump().span;
        self.expect_op("(")?;
        let cond = self.expr()?;
        self.expect_op(")")?;
        let then = self.gen_block()?;
        let otherwise = if self.eat_kw("else") { Some(self.gen_block()?) } else { None };
        Ok(Item::GenIf { cond, then, otherwise, span: start.to(self.prev_span()) })
    }

    fn gen_case(&mut self) -> PResult<Item> {
        let start = self.bump().span;
        self.expect_op("(")?;
        let expr = self.expr()?;
        self.expect_op(")")?;
        let mut items = Vec::new();
        while !self.eat_kw("endcase") {
            if self.at_eof() {
                return self.error("expected `endcase`");
            }
            let mut exprs = Vec::new();
            if self.eat_kw("default") {
                self.eat_op(":");
            } else {
                loop {
                    exprs.push(self.expr()?);
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op(":")?;
            }
            let body = self.gen_block()?;
            items.push(GenCaseItem { exprs, body });
        }
        Ok(Item::GenCase { expr, items, span: start.to(self.prev_span()) })
    }

    fn subroutine(&mut self, is_function: bool) -> PResult<SubroutineDecl> {
        let start = self.bump().span;
        let automatic = self.eat_kw("automatic");
        let (mut signed, mut range, mut ret_kind) = (false, None, None);
        if is_function {
            if self.is_kw("integer")
                || self.is_kw("real")
                || self.is_kw("time")
                || self.is_kw("reg")
                || self.is_kw("logic")
            {
                ret_kind = self.net_kind();
            }
            signed = self.eat_kw("signed");
            self.eat_kw("unsigned");
            range = self.opt_range()?;
        }
        let name = self.ident()?;
        let mut ports = Vec::new();
        if self.eat_op("(") {
            // ANSI-style argument list.
            if !self.is_op(")") {
                if let Ports::Ansi(p) = self.port_list()? {
                    ports = p;
                } else {
                    return self.error("expected argument declarations");
                }
            }
            self.expect_op(")")?;
        }
        self.expect_op(";")?;
        let mut decls = Vec::new();
        // Declarations, then a single statement.
        loop {
            let attrs = self.attributes()?;
            let s = self.span();
            match self.tok() {
                Tok::Ident(w) if matches!(w.as_str(), "input" | "output" | "inout") => {
                    let dir = self.direction().unwrap();
                    let kind = self.net_kind();
                    let sg = self.eat_kw("signed");
                    let r = self.opt_range()?;
                    let mut names = Vec::new();
                    loop {
                        names.push((self.ident()?, None));
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op(";")?;
                    ports.push(PortDecl {
                        attrs,
                        dir,
                        kind,
                        signed: sg,
                        range: r,
                        names,
                        span: s.to(self.prev_span()),
                    });
                }
                Tok::Ident(w)
                    if matches!(
                        w.as_str(),
                        "reg" | "integer" | "time" | "real" | "realtime" | "logic" | "wire"
                    ) =>
                {
                    decls.push(Item::Var(self.var_decl(attrs)?));
                }
                Tok::Ident(w) if matches!(w.as_str(), "parameter" | "localparam") => {
                    if let Some(mut it) = self.module_item()? {
                        decls.append(&mut it);
                    }
                }
                _ => break,
            }
        }
        let body = if self.is_kw(if is_function { "endfunction" } else { "endtask" }) {
            Stmt::Null(self.span())
        } else {
            self.stmt()?
        };
        // Verilog allows only one statement, but tolerate several.
        let mut stmts = vec![body];
        let end_kw = if is_function { "endfunction" } else { "endtask" };
        while !self.is_kw(end_kw) {
            if self.at_eof() {
                return self.error(format!("expected `{end_kw}`"));
            }
            stmts.push(self.stmt()?);
        }
        self.bump();
        if self.eat_op(":") {
            self.ident()?;
        }
        let body = if stmts.len() == 1 {
            stmts.pop().unwrap()
        } else {
            Stmt::Block { name: None, decls: vec![], span: start.to(self.prev_span()), stmts }
        };
        Ok(SubroutineDecl {
            automatic,
            name,
            signed,
            range,
            ret_kind,
            ports,
            decls,
            body,
            span: start.to(self.prev_span()),
        })
    }

    // ---------- statements ----------

    fn stmt(&mut self) -> PResult<Stmt> {
        let attrs = self.attributes()?;
        let start = self.span();
        match self.tok() {
            Tok::Op(";") => {
                self.bump();
                Ok(Stmt::Null(start))
            }
            Tok::Op("@") => {
                self.bump();
                let control = if self.eat_op("*") {
                    EventControl::Star
                } else {
                    self.expect_op("(")?;
                    let c = if self.is_op("*") {
                        self.bump();
                        EventControl::Star
                    } else {
                        let mut list = Vec::new();
                        loop {
                            let edge = if self.eat_kw("posedge") {
                                Edge::Posedge
                            } else if self.eat_kw("negedge") {
                                Edge::Negedge
                            } else {
                                Edge::Any
                            };
                            let expr = self.expr()?;
                            list.push(EventExpr { edge, expr });
                            if !(self.eat_op(",") || self.eat_kw("or")) {
                                break;
                            }
                        }
                        EventControl::List(list)
                    };
                    self.expect_op(")")?;
                    c
                };
                let body = self.stmt()?;
                Ok(Stmt::Timing { control, body: Box::new(body), span: start.to(self.prev_span()) })
            }
            Tok::Op("#") => {
                self.skip_delay()?;
                let body = self.stmt()?;
                Ok(Stmt::Delay { body: Box::new(body), span: start.to(self.prev_span()) })
            }
            Tok::SysIdent(n) => {
                let name = Ident { name: n.clone(), span: self.bump().span };
                let args = self.call_args()?;
                self.expect_op(";")?;
                Ok(Stmt::SysTask { name, args, span: start.to(self.prev_span()) })
            }
            Tok::Ident(w) => match w.as_str() {
                "begin" => {
                    self.bump();
                    let name = if self.eat_op(":") { Some(self.ident()?) } else { None };
                    let mut decls = Vec::new();
                    let mut stmts = Vec::new();
                    while !self.is_kw("end") {
                        if self.at_eof() {
                            return self.error("expected `end`");
                        }
                        let save = self.pos;
                        let a = self.attributes()?;
                        if matches!(self.tok(), Tok::Ident(w) if matches!(w.as_str(), "reg" | "integer" | "time" | "real" | "realtime" | "logic"))
                        {
                            decls.push(Item::Var(self.var_decl(a)?));
                            continue;
                        }
                        if self.is_kw("parameter") || self.is_kw("localparam") {
                            if let Some(mut it) = self.module_item()? {
                                decls.append(&mut it);
                            }
                            continue;
                        }
                        self.pos = save;
                        match self.stmt() {
                            Ok(s) => stmts.push(s),
                            Err(Reported) => self.recover_item(),
                        }
                    }
                    self.bump();
                    if self.eat_op(":") {
                        self.ident()?;
                    }
                    Ok(Stmt::Block { name, decls, stmts, span: start.to(self.prev_span()) })
                }
                "if" => {
                    self.bump();
                    self.expect_op("(")?;
                    let cond = self.expr()?;
                    self.expect_op(")")?;
                    let then = Box::new(self.stmt()?);
                    let otherwise =
                        if self.eat_kw("else") { Some(Box::new(self.stmt()?)) } else { None };
                    Ok(Stmt::If { cond, then, otherwise, attrs, span: start.to(self.prev_span()) })
                }
                "unique" | "priority" => {
                    self.bump();
                    self.stmt()
                }
                "case" | "casez" | "casex" => {
                    let kind = match w.as_str() {
                        "case" => CaseKind::Case,
                        "casez" => CaseKind::Casez,
                        _ => CaseKind::Casex,
                    };
                    self.bump();
                    self.expect_op("(")?;
                    let expr = self.expr()?;
                    self.expect_op(")")?;
                    let mut items = Vec::new();
                    while !self.eat_kw("endcase") {
                        if self.at_eof() {
                            return self.error("expected `endcase`");
                        }
                        let mut exprs = Vec::new();
                        if self.eat_kw("default") {
                            self.eat_op(":");
                        } else {
                            loop {
                                exprs.push(self.expr()?);
                                if !self.eat_op(",") {
                                    break;
                                }
                            }
                            self.expect_op(":")?;
                        }
                        let body = self.stmt()?;
                        items.push(CaseItem { exprs, body });
                    }
                    Ok(Stmt::Case { kind, expr, items, attrs, span: start.to(self.prev_span()) })
                }
                "for" => {
                    self.bump();
                    self.expect_op("(")?;
                    // `integer i = 0` (SystemVerilog) is accepted as `i = 0`.
                    if self.is_kw("integer") || self.is_kw("int") {
                        self.bump();
                    }
                    let init = self.simple_assign()?;
                    self.expect_op(";")?;
                    let cond = self.expr()?;
                    self.expect_op(";")?;
                    let var = self.ident()?;
                    let rhs = self.step_expr(&var)?;
                    let step = Stmt::Assign {
                        lhs: Expr::Ident(vec![(var.clone(), None)], var.span),
                        rhs,
                        nonblocking: false,
                        span: var.span,
                    };
                    self.expect_op(")")?;
                    let body = self.stmt()?;
                    Ok(Stmt::For {
                        init: Box::new(init),
                        cond,
                        step: Box::new(step),
                        body: Box::new(body),
                        span: start.to(self.prev_span()),
                    })
                }
                "while" => {
                    self.bump();
                    self.expect_op("(")?;
                    let cond = self.expr()?;
                    self.expect_op(")")?;
                    let body = Box::new(self.stmt()?);
                    Ok(Stmt::While { cond, body, span: start.to(self.prev_span()) })
                }
                "repeat" => {
                    self.bump();
                    self.expect_op("(")?;
                    let count = self.expr()?;
                    self.expect_op(")")?;
                    let body = Box::new(self.stmt()?);
                    Ok(Stmt::Repeat { count, body, span: start.to(self.prev_span()) })
                }
                "forever" => {
                    self.bump();
                    let body = Box::new(self.stmt()?);
                    Ok(Stmt::Forever { body, span: start.to(self.prev_span()) })
                }
                "disable" => {
                    self.bump();
                    let name = self.ident()?;
                    self.expect_op(";")?;
                    Ok(Stmt::Disable { name, span: start.to(self.prev_span()) })
                }
                "assign" | "deassign" | "force" | "release" | "fork" | "wait" => {
                    self.error(format!("`{w}` is not supported in synthesizable procedures"))
                }
                _ if is_keyword(w) => self.error("expected a statement"),
                _ => {
                    // Task call or assignment.
                    let is_call = matches!(self.tok_at(1), Tok::Op(";"))
                        || (matches!(self.tok_at(1), Tok::Op("(")) && self.looks_like_call());
                    if is_call {
                        let name = self.ident()?;
                        let args = self.call_args()?;
                        self.expect_op(";")?;
                        return Ok(Stmt::TaskCall { name, args, span: start.to(self.prev_span()) });
                    }
                    let lhs = self.lvalue()?;
                    let nonblocking = if self.eat_op("<=") {
                        true
                    } else if self.eat_op("=") {
                        false
                    } else if self.is_op("+=")
                        || self.is_op("-=")
                        || self.is_op("++")
                        || self.is_op("--")
                    {
                        let Some(var) = lhs.as_simple_ident().cloned() else {
                            return self.error("compound assignment needs a simple variable");
                        };
                        let rhs = self.step_expr(&var)?;
                        self.expect_op(";")?;
                        return Ok(Stmt::Assign {
                            lhs,
                            rhs,
                            nonblocking: false,
                            span: start.to(self.prev_span()),
                        });
                    } else {
                        return self.error("expected `=` or `<=`");
                    };
                    self.skip_delay()?;
                    let rhs = self.expr()?;
                    self.expect_op(";")?;
                    Ok(Stmt::Assign { lhs, rhs, nonblocking, span: start.to(self.prev_span()) })
                }
            },
            Tok::Op("{") => {
                let lhs = self.lvalue()?;
                let nonblocking = if self.eat_op("<=") {
                    true
                } else {
                    self.expect_op("=")?;
                    false
                };
                self.skip_delay()?;
                let rhs = self.expr()?;
                self.expect_op(";")?;
                Ok(Stmt::Assign { lhs, rhs, nonblocking, span: start.to(self.prev_span()) })
            }
            _ => self.error("expected a statement"),
        }
    }

    /// After `name (`, decides between a task call and an assignment to a
    /// function result (impossible) — a call ends with `);`.
    fn looks_like_call(&self) -> bool {
        let mut depth = 0;
        let mut k = 1;
        loop {
            match self.tok_at(k) {
                Tok::Op("(") => depth += 1,
                Tok::Op(")") => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(self.tok_at(k + 1), Tok::Op(";"));
                    }
                }
                Tok::Eof => return false,
                _ => {}
            }
            k += 1;
        }
    }

    fn simple_assign(&mut self) -> PResult<Stmt> {
        let start = self.span();
        let lhs = self.lvalue()?;
        self.expect_op("=")?;
        let rhs = self.expr()?;
        Ok(Stmt::Assign { lhs, rhs, nonblocking: false, span: start.to(self.prev_span()) })
    }

    fn call_args(&mut self) -> PResult<Vec<Expr>> {
        let mut args = Vec::new();
        if self.eat_op("(") {
            if !self.is_op(")") {
                loop {
                    if self.is_op(",") {
                        // Empty argument.
                        args.push(Expr::Str(String::new(), self.span()));
                    } else {
                        args.push(self.expr()?);
                    }
                    if !self.eat_op(",") {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
        }
        Ok(args)
    }

    /// An assignment target: identifier with selects, or a concatenation.
    fn lvalue(&mut self) -> PResult<Expr> {
        if self.is_op("{") {
            let start = self.bump().span;
            let mut parts = vec![self.lvalue()?];
            while self.eat_op(",") {
                parts.push(self.lvalue()?);
            }
            let end = self.expect_op("}")?;
            return Ok(Expr::Concat(parts, start.to(end)));
        }
        self.name_with_selects()
    }

    // ---------- expressions ----------

    fn expr(&mut self) -> PResult<Expr> {
        let cond = self.binary(0)?;
        if self.eat_op("?") {
            self.attributes()?;
            let then = self.expr()?;
            self.expect_op(":")?;
            let otherwise = self.expr()?;
            let span = cond.span().to(otherwise.span());
            return Ok(Expr::Ternary {
                cond: Box::new(cond),
                then: Box::new(then),
                otherwise: Box::new(otherwise),
                span,
            });
        }
        Ok(cond)
    }

    fn binop(&self) -> Option<(BinaryOp, u8)> {
        let Tok::Op(o) = self.tok() else { return None };
        Some(match *o {
            "||" => (BinaryOp::LogicOr, 1),
            "&&" => (BinaryOp::LogicAnd, 2),
            "|" => (BinaryOp::BitOr, 3),
            "^" => (BinaryOp::BitXor, 4),
            "~^" | "^~" => (BinaryOp::BitXnor, 4),
            "&" => (BinaryOp::BitAnd, 5),
            "==" => (BinaryOp::Eq, 6),
            "!=" => (BinaryOp::Ne, 6),
            "===" => (BinaryOp::CaseEq, 6),
            "!==" => (BinaryOp::CaseNe, 6),
            "<" => (BinaryOp::Lt, 7),
            "<=" => (BinaryOp::Le, 7),
            ">" => (BinaryOp::Gt, 7),
            ">=" => (BinaryOp::Ge, 7),
            "<<" => (BinaryOp::Shl, 8),
            ">>" => (BinaryOp::Shr, 8),
            "<<<" => (BinaryOp::AShl, 8),
            ">>>" => (BinaryOp::AShr, 8),
            "+" => (BinaryOp::Add, 9),
            "-" => (BinaryOp::Sub, 9),
            "*" => (BinaryOp::Mul, 10),
            "/" => (BinaryOp::Div, 10),
            "%" => (BinaryOp::Mod, 10),
            "**" => (BinaryOp::Pow, 11),
            _ => return None,
        })
    }

    fn binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.unary()?;
        while let Some((op, prec)) = self.binop() {
            if prec < min_prec {
                break;
            }
            self.bump();
            self.attributes()?;
            // All binary operators are left associative in IEEE 1364-2005.
            let rhs = self.binary(prec + 1)?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs), span };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let op = match self.tok() {
            Tok::Op("+") => Some(UnaryOp::Plus),
            Tok::Op("-") => Some(UnaryOp::Minus),
            Tok::Op("!") => Some(UnaryOp::LogicNot),
            Tok::Op("~") => Some(UnaryOp::BitNot),
            Tok::Op("&") => Some(UnaryOp::RedAnd),
            Tok::Op("~&") => Some(UnaryOp::RedNand),
            Tok::Op("|") => Some(UnaryOp::RedOr),
            Tok::Op("~|") => Some(UnaryOp::RedNor),
            Tok::Op("^") => Some(UnaryOp::RedXor),
            Tok::Op("~^") | Tok::Op("^~") => Some(UnaryOp::RedXnor),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            self.attributes()?;
            let e = self.unary()?;
            let span = start.to(e.span());
            return Ok(Expr::Unary { op, expr: Box::new(e), span });
        }
        self.primary()
    }

    fn number(&mut self) -> PResult<Expr> {
        let t = self.bump();
        Ok(match &t.tok {
            Tok::Decimal(d) => Expr::Number(
                Number { size: None, signed: true, base: None, digits: d.clone() },
                t.span,
            ),
            Tok::Based { size, signed, base, digits } => Expr::Number(
                Number { size: *size, signed: *signed, base: Some(*base), digits: digits.clone() },
                t.span,
            ),
            Tok::Real(r) => Expr::Real(r.clone(), t.span),
            _ => unreachable!(),
        })
    }

    fn primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let e = match self.tok() {
            Tok::Decimal(_) | Tok::Based { .. } | Tok::Real(_) => self.number()?,
            Tok::Str(s) => {
                let s = s.clone();
                self.bump();
                Expr::Str(s, start)
            }
            Tok::Op("(") => {
                self.bump();
                let e = self.expr()?;
                // Min:typ:max expressions keep the typical value.
                if self.eat_op(":") {
                    let typ = self.expr()?;
                    self.expect_op(":")?;
                    self.expr()?;
                    self.expect_op(")")?;
                    return Ok(typ);
                }
                self.expect_op(")")?;
                e
            }
            Tok::Op("{") => {
                self.bump();
                let first = self.expr()?;
                if self.is_op("{") {
                    // Replication: { count { a, b } }
                    self.bump();
                    let mut exprs = vec![self.expr()?];
                    while self.eat_op(",") {
                        exprs.push(self.expr()?);
                    }
                    self.expect_op("}")?;
                    let end = self.expect_op("}")?;
                    Expr::Repeat { count: Box::new(first), exprs, span: start.to(end) }
                } else {
                    let mut parts = vec![first];
                    while self.eat_op(",") {
                        parts.push(self.expr()?);
                    }
                    let end = self.expect_op("}")?;
                    Expr::Concat(parts, start.to(end))
                }
            }
            Tok::SysIdent(n) => {
                let name = Ident { name: n.clone(), span: self.bump().span };
                let args = if self.is_op("(") { self.call_args()? } else { Vec::new() };
                Expr::SysCall { name, args, span: start.to(self.prev_span()) }
            }
            Tok::Ident(n) if !is_keyword(n) => {
                if matches!(self.tok_at(1), Tok::Op("(")) {
                    let name = self.ident()?;
                    self.attributes()?;
                    let args = self.call_args()?;
                    Expr::Call { name, args, span: start.to(self.prev_span()) }
                } else {
                    return self.name_with_selects();
                }
            }
            _ => return self.error("expected an expression"),
        };
        self.selects(e)
    }

    /// A possibly hierarchical name followed by bit/part selects.
    fn name_with_selects(&mut self) -> PResult<Expr> {
        let start = self.span();
        let mut parts: Vec<(Ident, Option<Box<Expr>>)> = vec![(self.ident()?, None)];
        // Hierarchical parts: name[idx].name
        loop {
            if self.is_op("[") && self.hier_index_follows() {
                self.bump();
                let idx = self.expr()?;
                self.expect_op("]")?;
                parts.last_mut().unwrap().1 = Some(Box::new(idx));
                self.expect_op(".")?;
                parts.push((self.ident()?, None));
            } else if self.is_op(".") && matches!(self.tok_at(1), Tok::Ident(_)) {
                self.bump();
                parts.push((self.ident()?, None));
            } else {
                break;
            }
        }
        let e = Expr::Ident(parts, start.to(self.prev_span()));
        self.selects(e)
    }

    /// Whether the `[ ... ]` at the cursor is followed by `.` (a generate
    /// block index in a hierarchical name).
    fn hier_index_follows(&self) -> bool {
        let mut depth = 0;
        let mut k = 0;
        loop {
            match self.tok_at(k) {
                Tok::Op("[") => depth += 1,
                Tok::Op("]") => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(self.tok_at(k + 1), Tok::Op("."))
                            && matches!(self.tok_at(k + 2), Tok::Ident(_));
                    }
                }
                Tok::Eof | Tok::Op(";") => return false,
                _ => {}
            }
            k += 1;
        }
    }

    fn selects(&mut self, mut e: Expr) -> PResult<Expr> {
        while self.is_op("[") {
            let start = e.span();
            self.bump();
            let first = self.expr()?;
            if self.eat_op(":") {
                let lsb = self.expr()?;
                let end = self.expect_op("]")?;
                e = Expr::Range {
                    base: Box::new(e),
                    msb: Box::new(first),
                    lsb: Box::new(lsb),
                    span: start.to(end),
                };
            } else if self.is_op("+:") || self.is_op("-:") {
                let up = self.is_op("+:");
                self.bump();
                let width = self.expr()?;
                let end = self.expect_op("]")?;
                e = Expr::IndexedRange {
                    base: Box::new(e),
                    start: Box::new(first),
                    width: Box::new(width),
                    up,
                    span: start.to(end),
                };
            } else {
                let end = self.expect_op("]")?;
                e = Expr::Index { base: Box::new(e), index: Box::new(first), span: start.to(end) };
            }
        }
        Ok(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preproc::{PreprocOptions, preprocess};
    use synth_core::SourceMap;

    fn parse_src(src: &str) -> (SourceText, Vec<Diagnostic>) {
        let mut sm = SourceMap::new();
        let f = sm.add("t.v", src);
        let pp = preprocess(&mut sm, &[f], &PreprocOptions::default());
        assert!(pp.diagnostics.is_empty(), "{:?}", pp.diagnostics);
        let p = parse(&pp.tokens);
        (p.source, p.diagnostics)
    }

    #[test]
    fn module_with_everything() {
        let (s, d) = parse_src(
            r#"
(* top *)
module counter #(parameter W = 8, parameter [3:0] Q = 4'h3) (
  input wire clk, rst,
  input [W-1:0] din,
  output reg [W-1:0] q = 0
);
  localparam X = W * 2 + 1;
  wire [7:0] a = din[3 +: 4] ^ {2{din[1:0]}};
  reg [7:0] mem [0:15];
  integer i;
  genvar g;
  assign out = (a > 3) ? a[0] : ~|a;
  always @(posedge clk or posedge rst)
    if (rst) q <= 0;
    else begin : blk
      q <= q + 1'b1;
      case (q)
        8'd0, 8'd1: q <= 2;
        default: ;
      endcase
      for (i = 0; i < 4; i = i + 1) mem[i] <= mem[i+1];
    end
  always @* begin
    {c, d} = a - b;
  end
  generate for (g = 0; g < 4; g = g + 1) begin : gen
    sub #(.N(g)) u (.a(a[g]), .b());
  end endgenerate
  function [3:0] f(input [3:0] x);
    f = x + 1;
  endfunction
  initial $readmemh("x.hex", mem);
  and g1(o, a, b);
endmodule
"#,
        );
        assert!(d.is_empty(), "{d:#?}");
        let m = &s.modules[0];
        assert_eq!(m.name.name, "counter");
        assert_eq!(m.params.len(), 2);
        let Ports::Ansi(ports) = &m.ports else { panic!() };
        assert_eq!(ports.len(), 3);
        assert_eq!(ports[0].names.len(), 2);
        assert!(m.items.iter().any(|i| matches!(i, Item::GenFor { .. })));
        assert!(m.items.iter().any(|i| matches!(i, Item::Function(_))));
        assert!(m.items.iter().any(|i| matches!(i, Item::Gate(_))));
    }

    #[test]
    fn precedence() {
        let (s, d) = parse_src(
            "module m; assign y = a + b * c << 1 == d & e | f && g || h ? i : j; endmodule",
        );
        assert!(d.is_empty(), "{d:?}");
        let Item::ContAssign { assigns, .. } = &s.modules[0].items[0] else { panic!() };
        let Expr::Ternary { cond, .. } = &assigns[0].1 else { panic!("{:?}", assigns[0].1) };
        let Expr::Binary { op: BinaryOp::LogicOr, .. } = cond.as_ref() else { panic!() };
    }

    #[test]
    fn non_ansi_and_errors() {
        let (s, d) =
            parse_src("module m(a, b); input a; output [1:0] b; assign b = {a, a}; endmodule");
        assert!(d.is_empty(), "{d:?}");
        assert!(matches!(&s.modules[0].ports, Ports::NonAnsi(v) if v.len() == 2));
        let (_, d) = parse_src("module m; assign = 1; wire ok; endmodule");
        assert_eq!(d.len(), 1, "{d:?}");
    }
}
