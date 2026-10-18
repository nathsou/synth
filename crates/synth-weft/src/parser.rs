//! Recursive-descent parser with Pratt expression parsing.
//!
//! Errors are reported with a label and, where a likely mistake is
//! recognisable (often a Verilog habit), a suggestion. After an error the
//! parser skips to the next statement or item and continues, so one run
//! reports several independent problems.

use synth_core::{BigInt, Diagnostic, Diagnostics, Span};

use crate::ast::*;
use crate::lexer::{Tok, Token};

pub const KEYWORDS: &[&str] = &[
    "async", "bool", "clock", "const", "default", "domain", "else", "enum", "extern", "fn", "for",
    "if", "in", "index", "init", "inout", "inst", "int", "let", "match", "mem", "module", "out",
    "reg", "sint", "string", "struct", "true", "false", "type", "uint", "undef", "var", "wire",
];

struct Parser<'a> {
    toks: Vec<Token>,
    pos: usize,
    diags: &'a mut Diagnostics,
    /// Current `{` nesting, for error recovery.
    depth: i32,
    /// Nesting at the start of each statement being parsed.
    stmt_depth: Vec<i32>,
}

type R<T> = Result<T, ()>;

pub fn parse(toks: &[Token], diags: &mut Diagnostics) -> File {
    let mut p = Parser { toks: toks.to_vec(), pos: 0, diags, depth: 0, stmt_depth: Vec::new() };
    let mut file = File::default();
    while !p.at_eof() {
        match p.item() {
            Ok(it) => file.items.push(it),
            Err(()) => p.recover_item(),
        }
    }
    file
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }

    fn span(&self) -> Span {
        self.toks[self.pos].span
    }

    fn prev_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1)].span
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }

    fn bump(&mut self) -> &Token {
        match self.toks[self.pos].tok {
            Tok::Punct("{") => self.depth += 1,
            Tok::Punct("}") => self.depth -= 1,
            _ => {}
        }
        let t = &self.toks[self.pos];
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    /// An identifier in a field position, where keywords are allowed.
    fn field_name(&mut self, what: &str) -> R<Ident> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                let span = self.bump().span;
                Ok(Ident { name, span })
            }
            _ => {
                self.unexpected(what);
                Err(())
            }
        }
    }

    fn is(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(q) if *q == p)
    }

    fn is_at(&self, n: usize, p: &str) -> bool {
        matches!(self.peek_at(n), Tok::Punct(q) if *q == p)
    }

    fn is_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s == k)
    }

    fn eat(&mut self, p: &str) -> bool {
        if self.is(p) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: &str) -> bool {
        if self.is_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn error(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    fn unexpected(&mut self, what: &str) {
        let t = self.peek().describe();
        let sp = self.span();
        self.error(
            Diagnostic::error(sp, format!("expected {what}, found {t}"))
                .with_label(format!("expected {what}")),
        );
    }

    /// `>`, or a `>>`/`>=` token whose first character closes a generic list.
    fn is_close_angle(&self) -> bool {
        self.is(">") || self.is(">>") || self.is(">=")
    }

    fn expect(&mut self, p: &'static str) -> R<Span> {
        if self.is(p) {
            return Ok(self.bump().span);
        }
        if p == ">" && (self.is(">>") || self.is(">=")) {
            let t = &mut self.toks[self.pos];
            let first = Span { file: t.span.file, start: t.span.start, end: t.span.start + 1 };
            t.tok = Tok::Punct(if t.tok == Tok::Punct(">>") { ">" } else { "=" });
            t.span.start += 1;
            return Ok(first);
        }
        let found = self.peek().describe();
        if p == ";" || p == "," {
            // Point just after the previous token: that is where it is missing.
            let prev = self.prev_span();
            let at = Span { file: prev.file, start: prev.end, end: prev.end };
            self.error(
                Diagnostic::error(at, format!("expected `{p}`, found {found}"))
                    .with_label(format!("add `{p}` here")),
            );
        } else {
            let sp = self.span();
            self.error(
                Diagnostic::error(sp, format!("expected `{p}`, found {found}"))
                    .with_label(format!("expected `{p}`")),
            );
        }
        Err(())
    }

    fn ident(&mut self, what: &str) -> R<Ident> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                if KEYWORDS.contains(&name.as_str()) {
                    let sp = self.span();
                    self.error(
                        Diagnostic::error(
                            sp,
                            format!("`{name}` is a keyword and cannot be used as {what}"),
                        )
                        .with_label("keyword")
                        .with_help(format!("pick another name, such as `{name}_`")),
                    );
                    return Err(());
                }
                let span = self.bump().span;
                Ok(Ident { name, span })
            }
            _ => {
                self.unexpected(what);
                Err(())
            }
        }
    }

    /// Skips to the start of the next item.
    fn recover_item(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.peek() {
                Tok::Eof => return,
                Tok::Punct("{") => depth += 1,
                Tok::Punct("}") => {
                    depth -= 1;
                    if depth <= 0 {
                        self.bump();
                        return;
                    }
                }
                Tok::Ident(k)
                    if depth <= 0
                        && matches!(
                            k.as_str(),
                            "module" | "extern" | "fn" | "struct" | "enum" | "const"
                        ) =>
                {
                    return;
                }
                _ => {}
            }
            self.bump();
        }
    }

    /// Skips past the end of the current statement: to its `;`, or to the
    /// `}` that closes a block it opened, or to the `}` of the enclosing block.
    fn recover_stmt(&mut self) {
        let base = self.stmt_depth.last().copied().unwrap_or(self.depth);
        loop {
            match self.peek() {
                Tok::Eof => return,
                Tok::Punct(";") if self.depth == base => {
                    self.bump();
                    return;
                }
                Tok::Punct("}") if self.depth == base => return,
                Tok::Punct("}") if self.depth == base + 1 => {
                    self.bump();
                    // A block-ending statement (`if`, `match`, `domain`) ends here.
                    if !self.is(";") && !self.is_kw("else") {
                        return;
                    }
                    continue;
                }
                _ => {}
            }
            self.bump();
        }
    }

    fn attributes(&mut self) -> R<Vec<Attribute>> {
        let mut out = Vec::new();
        while self.eat("#[") {
            let name = self.ident("an attribute name")?;
            let mut args = Vec::new();
            if self.eat("(") {
                while !self.is(")") {
                    args.push(self.expr()?);
                    if !self.eat(",") {
                        break;
                    }
                }
                self.expect(")")?;
            }
            self.expect("]")?;
            out.push(Attribute { name, args });
        }
        Ok(out)
    }

    fn item(&mut self) -> R<Item> {
        let attrs = self.attributes()?;
        let start = self.span();
        let kind = match self.peek() {
            Tok::Ident(k) if k == "module" => {
                self.bump();
                ItemKind::Module(self.module(false, start)?)
            }
            Tok::Ident(k) if k == "extern" => {
                self.bump();
                if !self.eat_kw("module") {
                    self.unexpected("`module` after `extern`");
                    return Err(());
                }
                ItemKind::Module(self.module(true, start)?)
            }
            Tok::Ident(k) if k == "fn" => {
                self.bump();
                ItemKind::Function(self.function(start)?)
            }
            Tok::Ident(k) if k == "struct" => {
                self.bump();
                ItemKind::Struct(self.struct_def()?)
            }
            Tok::Ident(k) if k == "enum" => {
                self.bump();
                ItemKind::Enum(self.enum_def()?)
            }
            Tok::Ident(k) if k == "const" => {
                self.bump();
                ItemKind::Const(self.const_decl()?)
            }
            Tok::Ident(k) if k == "assert" => {
                self.bump();
                ItemKind::Assert(self.assert_rest()?)
            }
            Tok::Ident(k) if k == "endmodule" || k == "input" || k == "output" || k == "wire" => {
                let sp = self.span();
                self.error(
                    Diagnostic::error(sp, format!("`{k}` cannot start an item"))
                        .with_label("unexpected here")
                        .with_help("is this Verilog? Weft files contain `module`, `fn`, `struct`, `enum` and `const` items; pass `.v` files separately"),
                );
                return Err(());
            }
            _ => {
                self.unexpected(
                    "an item (`module`, `extern module`, `fn`, `struct`, `enum` or `const`)",
                );
                return Err(());
            }
        };
        Ok(Item { kind, attrs, span: start.to(self.prev_span()) })
    }

    fn generics(&mut self) -> R<Vec<Generic>> {
        let mut out = Vec::new();
        if !self.eat("<") {
            return Ok(out);
        }
        while !self.is(">") {
            let name = self.ident("a generic parameter name")?;
            self.expect(":")?;
            let kind = match self.peek() {
                Tok::Ident(k) if k == "int" => GenericKind::Int,
                Tok::Ident(k) if k == "bool" => GenericKind::Bool,
                Tok::Ident(k) if k == "string" => GenericKind::Str,
                Tok::Ident(k) if k == "type" => GenericKind::Type,
                _ => {
                    self.unexpected("`int`, `bool`, `string` or `type`");
                    return Err(());
                }
            };
            self.bump();
            let default = if self.eat("=") { Some(self.gen_arg_value()?) } else { None };
            out.push(Generic { name, kind, default });
            if !self.eat(",") {
                break;
            }
        }
        self.expect(">")?;
        Ok(out)
    }

    fn module(&mut self, is_extern: bool, start: Span) -> R<Module> {
        let name = self.ident("a module name")?;
        let generics = self.generics()?;
        self.expect("(")?;
        let mut ports = Vec::new();
        while !self.is(")") {
            match self.port(is_extern) {
                Ok(p) => ports.push(p),
                Err(()) => {
                    // Skip to the next port.
                    while !self.is(",") && !self.is(")") && !self.at_eof() && !self.is("{") {
                        self.bump();
                    }
                }
            }
            if !self.eat(",") {
                break;
            }
        }
        self.expect(")")?;
        let body = if is_extern {
            self.expect(";")?;
            None
        } else {
            if self.is(";") {
                let sp = self.span();
                self.error(
                    Diagnostic::error(sp, "a module needs a body")
                        .with_label("expected `{`")
                        .with_help(
                            "to declare a module defined in Verilog, write `extern module ...;`",
                        ),
                );
                return Err(());
            }
            Some(self.block()?)
        };
        Ok(Module { name, generics, ports, body, span: start.to(self.prev_span()) })
    }

    fn port(&mut self, is_extern: bool) -> R<Port> {
        let attrs = self.attributes()?;
        let start = self.span();
        if let Tok::Ident(k) = self.peek().clone() {
            match k.as_str() {
                "in" | "out" => {
                    self.bump();
                    let name = self.ident("a port name")?;
                    self.expect(":")?;
                    let ty = self.ty()?;
                    let dom = self.dom_opt()?;
                    let kind = if k == "in" {
                        let default = if self.eat("=") { Some(self.expr()?) } else { None };
                        PortKind::In { ty, dom, default }
                    } else {
                        if self.is("=") {
                            let sp = self.span();
                            self.error(
                                Diagnostic::error(sp, "output ports cannot have a default value")
                                    .with_label("not allowed on `out`")
                                    .with_help("drive the output in the module body"),
                            );
                            return Err(());
                        }
                        PortKind::Out { ty, dom }
                    };
                    return Ok(Port { name, kind, attrs, span: start.to(self.prev_span()) });
                }
                "inout" => {
                    self.bump();
                    let name = self.ident("a port name")?;
                    self.expect(":")?;
                    let ty = self.ty()?;
                    return Ok(Port {
                        name,
                        kind: PortKind::Inout { ty },
                        attrs,
                        span: start.to(self.prev_span()),
                    });
                }
                "default" | "domain" => {
                    let default = self.eat_kw("default");
                    if !self.eat_kw("domain") {
                        self.unexpected("`domain`");
                        return Err(());
                    }
                    let name = if is_extern && self.is("{") {
                        Ident { name: "default".into(), span: self.prev_span() }
                    } else {
                        self.ident("a domain name")?
                    };
                    if self.is("{") {
                        if !is_extern {
                            let sp = self.span();
                            self.error(
                                Diagnostic::error(sp, "domain bindings are only allowed in extern modules")
                                    .with_label("unexpected `{`")
                                    .with_help("a native module receives a domain with `domain name`, and defines one in its body"),
                            );
                            return Err(());
                        }
                        self.bump();
                        let (mut clock, mut reset) = (None, None);
                        while !self.is("}") {
                            let field = self.field_name("`clock` or `reset`")?;
                            self.expect(":")?;
                            let low = self.eat("!");
                            let port = self.ident("a port name")?;
                            match field.name.as_str() {
                                "clock" if !low => clock = Some(port),
                                "reset" => reset = Some((port, low)),
                                _ => {
                                    self.error(
                                        Diagnostic::error(
                                            field.span,
                                            format!("unknown binding `{}`", field.name),
                                        )
                                        .with_label("expected `clock` or `reset`"),
                                    );
                                    return Err(());
                                }
                            }
                            if !self.eat(",") {
                                break;
                            }
                        }
                        self.expect("}")?;
                        let Some(clock) = clock else {
                            self.error(
                                Diagnostic::error(start, "a domain binding needs a `clock` port")
                                    .with_label("missing `clock: ...`"),
                            );
                            return Err(());
                        };
                        return Ok(Port {
                            name,
                            kind: PortKind::Binding { clock, reset },
                            attrs,
                            span: start.to(self.prev_span()),
                        });
                    }
                    return Ok(Port {
                        name,
                        kind: PortKind::Domain { default },
                        attrs,
                        span: start.to(self.prev_span()),
                    });
                }
                "input" | "output" => {
                    let sp = self.span();
                    let w = if k == "input" { "in" } else { "out" };
                    self.error(
                        Diagnostic::error(
                            sp,
                            format!("`{k}` is Verilog; Weft ports are declared with `{w}`"),
                        )
                        .with_label("not a Weft port direction")
                        .with_help(format!(
                            "write `{w} name: type`, for example `{w} led: uint<8>`"
                        )),
                    );
                    return Err(());
                }
                _ => {}
            }
        }
        self.unexpected("a port (`in`, `out`, `inout` or `domain`)");
        Err(())
    }

    fn dom_opt(&mut self) -> R<Option<DomRef>> {
        if !self.is("@") {
            return Ok(None);
        }
        let start = self.bump().span;
        let kind = match self.peek().clone() {
            Tok::Ident(k) if k == "async" => {
                self.bump();
                DomRefKind::Async
            }
            Tok::Ident(k) if k == "default" => {
                self.bump();
                DomRefKind::Default
            }
            _ => DomRefKind::Named(self.ident("a domain name")?),
        };
        Ok(Some(DomRef { kind, span: start.to(self.prev_span()) }))
    }

    fn function(&mut self, start: Span) -> R<Function> {
        let name = self.ident("a function name")?;
        let generics = self.generics()?;
        self.expect("(")?;
        let mut params = Vec::new();
        while !self.is(")") {
            let n = self.ident("a parameter name")?;
            self.expect(":")?;
            let t = self.ty()?;
            params.push((n, t));
            if !self.eat(",") {
                break;
            }
        }
        self.expect(")")?;
        if !self.eat("->") {
            let sp = self.span();
            self.error(
                Diagnostic::error(sp, "a function needs a result type")
                    .with_label("expected `->`")
                    .with_help("write `fn name(x: uint<8>) -> bool { ... }`"),
            );
            return Err(());
        }
        let ret = self.ty()?;
        let open = self.expect("{")?;
        let (body, result) = self.fn_body(open)?;
        Ok(Function { name, generics, params, ret, body, result, span: start.to(self.prev_span()) })
    }

    /// The statements and result expression of a function body, after `{`.
    fn fn_body(&mut self, open: Span) -> R<(Vec<Stmt>, Expr)> {
        let mut stmts = Vec::new();
        loop {
            if self.is("}") {
                let sp = self.span();
                self.error(
                    Diagnostic::error(sp, "a function body must end with its result")
                        .with_label("expected an expression before `}`")
                        .with_secondary(open, "function body starts here")
                        .with_help("the last line of a function is its value, without `;`"),
                );
                return Err(());
            }
            if self.at_eof() {
                self.unexpected("`}`");
                return Err(());
            }
            let save = self.pos;
            if self.is_kw("if") || self.is_kw("match") {
                // A trailing `if`/`match` is the result; earlier ones are statements.
                let ndiag = self.diags.items.len();
                let st = self.stmt();
                if self.is("}") {
                    self.diags.items.truncate(ndiag);
                    self.pos = save;
                    let e = self.expr()?;
                    self.expect("}")?;
                    return Ok((stmts, e));
                }
                if let Ok(s) = st {
                    stmts.push(s);
                }
                continue;
            }
            let starts_stmt = matches!(self.peek(), Tok::Ident(k) if matches!(k.as_str(), "let" | "var" | "const" | "for" | "assert"));
            if starts_stmt || self.is("#[") {
                if let Ok(s) = self.stmt() {
                    stmts.push(s);
                }
                continue;
            }
            let e = match self.expr() {
                Ok(e) => e,
                Err(()) => {
                    self.recover_stmt();
                    continue;
                }
            };
            if self.eat("}") {
                return Ok((stmts, e));
            }
            if self.is("=") || self.is(":=") {
                let op = if self.bump().tok == Tok::Punct("=") {
                    AssignOp::Drive
                } else {
                    AssignOp::Next
                };
                let value = match self.expr() {
                    Ok(v) => v,
                    Err(()) => {
                        self.recover_stmt();
                        continue;
                    }
                };
                let span = e.span.to(self.span());
                if self.expect(";").is_err() {
                    self.recover_stmt();
                }
                stmts.push(Stmt {
                    kind: StmtKind::Assign { target: e, op, value },
                    attrs: vec![],
                    span,
                });
                continue;
            }
            if self.is(";") {
                let sp = self.span();
                self.error(
                    Diagnostic::error(e.span, "this expression's value is not used")
                        .with_label("unused value")
                        .with_secondary(sp, "the `;` ends the statement")
                        .with_help("if this is the function's result, remove the `;`"),
                );
                self.bump();
                continue;
            }
            self.unexpected("`;`, `=` or `}`");
            self.recover_stmt();
        }
    }

    fn struct_def(&mut self) -> R<StructDef> {
        let name = self.ident("a struct name")?;
        self.expect("{")?;
        let mut fields = Vec::new();
        while !self.is("}") {
            let f = self.ident("a field name")?;
            self.expect(":")?;
            let t = self.ty()?;
            fields.push((f, t));
            if !self.eat(",") {
                break;
            }
        }
        self.expect("}")?;
        Ok(StructDef { name, fields })
    }

    fn enum_def(&mut self) -> R<EnumDef> {
        let name = self.ident("an enum name")?;
        let repr = if self.eat(":") { Some(self.ty()?) } else { None };
        self.expect("{")?;
        let mut variants = Vec::new();
        while !self.is("}") {
            let v = self.ident("a variant name")?;
            let val = if self.eat("=") { Some(self.expr()?) } else { None };
            variants.push((v, val));
            if !self.eat(",") {
                break;
            }
        }
        self.expect("}")?;
        Ok(EnumDef { name, repr, variants })
    }

    fn const_decl(&mut self) -> R<ConstDecl> {
        let name = self.ident("a constant name")?;
        let mut kind = None;
        let mut ty = None;
        if self.eat(":") {
            match self.peek() {
                Tok::Ident(k) if k == "int" || k == "string" => {
                    let k = if k == "int" { GenericKind::Int } else { GenericKind::Str };
                    kind = Some((k, self.bump().span));
                }
                _ => ty = Some(self.ty()?),
            }
        }
        self.expect("=")?;
        let value = self.expr()?;
        self.expect(";")?;
        Ok(ConstDecl { name, ty, kind, value })
    }

    fn assert_rest(&mut self) -> R<Assert> {
        self.expect("(")?;
        let cond = self.expr()?;
        self.expect(",")?;
        let message = match self.peek().clone() {
            Tok::Str(s) => {
                self.bump();
                s
            }
            _ => {
                self.unexpected("a message string");
                return Err(());
            }
        };
        self.expect(")")?;
        self.expect(";")?;
        Ok(Assert { cond, message })
    }

    fn ty(&mut self) -> R<TypeExpr> {
        let start = self.span();
        let kind = match self.peek().clone() {
            Tok::Ident(k) if k == "bool" => {
                self.bump();
                TypeKind::Bool
            }
            Tok::Ident(k) if k == "clock" => {
                self.bump();
                TypeKind::Clock
            }
            Tok::Ident(k) if k == "uint" || k == "sint" || k == "index" => {
                self.bump();
                if !self.is("<") {
                    let sp = self.span();
                    self.error(
                        Diagnostic::error(sp, format!("`{k}` needs a size"))
                            .with_label("expected `<`")
                            .with_help(format!("write `{k}<8>`")),
                    );
                    return Err(());
                }
                self.bump();
                let e = self.expr_bp(BP_SHIFT + 1)?;
                self.expect(">")?;
                let e = Box::new(e);
                match k.as_str() {
                    "uint" => TypeKind::Uint(e),
                    "sint" => TypeKind::Sint(e),
                    _ => TypeKind::Index(e),
                }
            }
            Tok::Punct("[") => {
                self.bump();
                let elem = self.ty()?;
                if !self.is(";") {
                    let sp = self.span();
                    self.error(
                        Diagnostic::error(sp, format!("expected `;` in array type, found {}", self.peek().describe()))
                            .with_label("expected `;`")
                            .with_help("array types are written `[element; count]`, for example `[uint<8>; 16]`"),
                    );
                    return Err(());
                }
                self.bump();
                let n = self.expr()?;
                self.expect("]")?;
                TypeKind::Array(Box::new(elem), Box::new(n))
            }
            Tok::Ident(k) if k == "reg" || k == "wire" || k == "logic" => {
                let sp = self.span();
                self.error(
                    Diagnostic::error(sp, format!("`{k}` is not a type"))
                        .with_label("expected a type")
                        .with_help("types are `bool`, `uint<N>`, `sint<N>`, `index<N>`, arrays, structs and enums; registers are declared with `reg name: type`"),
                );
                return Err(());
            }
            Tok::Ident(k) if k == "int" || k == "string" || k == "type" => {
                let sp = self.span();
                self.error(
                    Diagnostic::error(sp, format!("`{k}` is a compile-time type, not a hardware type"))
                        .with_label("not a hardware type")
                        .with_help(format!(
                            "`{k}` is only for generic parameters and constants: `fn f<N: {k}>(…)`, `const X: {k} = …;`"
                        ))
                        .with_help("for an integer signal, use `uint<N>`, `sint<N>` or `index<N>`"),
                );
                return Err(());
            }
            Tok::Ident(_) => TypeKind::Named(self.ident("a type")?),
            _ => {
                self.unexpected("a type");
                return Err(());
            }
        };
        Ok(TypeExpr { kind, span: start.to(self.prev_span()) })
    }

    fn block(&mut self) -> R<Vec<Stmt>> {
        self.expect("{")?;
        let mut out = Vec::new();
        while !self.is("}") && !self.at_eof() {
            if let Ok(s) = self.stmt() {
                out.push(s);
            }
        }
        self.expect("}")?;
        Ok(out)
    }

    fn stmt(&mut self) -> R<Stmt> {
        self.stmt_depth.push(self.depth);
        let r = self.stmt_inner();
        if r.is_err() {
            self.recover_stmt();
        }
        self.stmt_depth.pop();
        r
    }

    fn stmt_inner(&mut self) -> R<Stmt> {
        let attrs = self.attributes()?;
        let start = self.span();
        let kind = self.stmt_kind()?;
        Ok(Stmt { kind, attrs, span: start.to(self.prev_span()) })
    }

    fn stmt_kind(&mut self) -> R<StmtKind> {
        if let Tok::Ident(k) = self.peek().clone() {
            match k.as_str() {
                "const" => {
                    self.bump();
                    return Ok(StmtKind::Const(self.const_decl()?));
                }
                "let" | "var" => {
                    self.bump();
                    let name = self.ident("a name")?;
                    let ty = if self.eat(":") { Some(self.ty()?) } else { None };
                    let dom = if k == "let" { self.dom_opt()? } else { None };
                    self.expect("=")?;
                    let value = self.expr()?;
                    self.expect(";")?;
                    return Ok(if k == "let" {
                        StmtKind::Let { name, ty, dom, value }
                    } else {
                        StmtKind::Var { name, ty, value }
                    });
                }
                "wire" => {
                    self.bump();
                    let name = self.ident("a wire name")?;
                    self.decl_colon(&name, "wire")?;
                    let ty = self.ty()?;
                    let dom = self.dom_opt()?;
                    if self.is("=") {
                        let sp = self.span();
                        self.error(
                            Diagnostic::error(sp, "a `wire` declaration has no value")
                                .with_label("unexpected `=`")
                                .with_help(format!("for a named value, write `let {} = ...;`; to drive a wire, assign it in a separate statement: `{} = ...;`", name.name, name.name)),
                        );
                        return Err(());
                    }
                    self.expect(";")?;
                    return Ok(StmtKind::Wire { name, ty, dom });
                }
                "reg" => {
                    self.bump();
                    let name = self.ident("a register name")?;
                    self.decl_colon(&name, "reg")?;
                    let ty = self.ty()?;
                    let dom = self.dom_opt()?;
                    let init_only = if self.eat_kw("init") {
                        true
                    } else if self.eat("=") {
                        false
                    } else {
                        let sp = self.span();
                        self.error(
                            Diagnostic::error(
                                sp,
                                format!("register `{}` needs a reset or power-on value", name.name),
                            )
                            .with_label("expected `=` or `init`")
                            .with_help(format!(
                                "`reg {}: ... = 0;` resets to 0 (and starts at 0)",
                                name.name
                            ))
                            .with_help(format!(
                                "`reg {}: ... init 0;` starts at 0 and is not affected by reset",
                                name.name
                            )),
                        );
                        return Err(());
                    };
                    let value = self.expr()?;
                    self.expect(";")?;
                    return Ok(StmtKind::Reg { name, ty, dom, value, init_only });
                }
                "mem" => {
                    self.bump();
                    let name = self.ident("a memory name")?;
                    self.decl_colon(&name, "mem")?;
                    let ty = self.ty()?;
                    let TypeKind::Array(elem, depth) = ty.kind else {
                        self.error(
                            Diagnostic::error(ty.span, "a memory's type is an array")
                                .with_label("expected `[element; depth]`")
                                .with_help("for example `mem ram: [uint<8>; 512];`"),
                        );
                        return Err(());
                    };
                    let dom = self.dom_opt()?;
                    let init = if self.eat_kw("init") { Some(self.expr()?) } else { None };
                    if self.is("=") {
                        let sp = self.span();
                        self.error(
                            Diagnostic::error(sp, "memories have no reset value")
                                .with_label("unexpected `=`")
                                .with_help("give the initial contents with `init`"),
                        );
                        return Err(());
                    }
                    self.expect(";")?;
                    return Ok(StmtKind::Mem { name, elem: *elem, depth: *depth, dom, init });
                }
                "inst" => {
                    self.bump();
                    let name = self.ident("an instance name")?;
                    self.expect("=")?;
                    let call = self.expr()?;
                    if !matches!(call.kind, ExprKind::Call { .. }) {
                        self.error(
                            Diagnostic::error(call.span, "expected a module instantiation")
                                .with_label("not a module call")
                                .with_help("write `inst name = module_name(port: value, ...);`"),
                        );
                        return Err(());
                    }
                    self.expect(";")?;
                    return Ok(StmtKind::Inst { name, call });
                }
                "default" | "domain" => {
                    let default = self.eat_kw("default");
                    if !self.eat_kw("domain") {
                        self.unexpected("`domain`");
                        return Err(());
                    }
                    let name = self.ident("a domain name")?;
                    self.expect("{")?;
                    let mut fields = Vec::new();
                    while !self.is("}") {
                        let fname = self.field_name("`clock`, `reset`, `async_reset` or `edge`")?;
                        self.expect(":")?;
                        let value = if fname.name == "edge" {
                            match self.peek().clone() {
                                Tok::Ident(e) if e == "rising" || e == "falling" => {
                                    self.bump();
                                    DomFieldValue::Edge(e == "rising")
                                }
                                _ => {
                                    self.unexpected("`rising` or `falling`");
                                    return Err(());
                                }
                            }
                        } else {
                            DomFieldValue::Expr(self.expr()?)
                        };
                        fields.push(DomField { name: fname, value });
                        if !self.eat(",") {
                            break;
                        }
                    }
                    self.expect("}")?;
                    self.eat(";");
                    return Ok(StmtKind::Domain { name, default, fields });
                }
                "if" => {
                    self.bump();
                    let cond = self.cond_expr()?;
                    let then = self.block()?;
                    let mut else_span = None;
                    let els = if self.is_kw("else") {
                        else_span = Some(self.bump().span);
                        if self.is_kw("if") {
                            Some(vec![self.stmt()?])
                        } else {
                            Some(self.block()?)
                        }
                    } else {
                        None
                    };
                    return Ok(StmtKind::If { cond, then, els, else_span });
                }
                "match" => {
                    self.bump();
                    let scrutinee = self.cond_expr()?;
                    self.expect("{")?;
                    let mut arms = Vec::new();
                    while !self.is("}") && !self.at_eof() {
                        self.stmt_depth.push(self.depth);
                        let arm = (|| {
                            let pat = self.pattern()?;
                            self.expect("=>")?;
                            let body = self.arm_stmt()?;
                            Ok::<_, ()>((pat, body))
                        })();
                        match arm {
                            Ok(a) => {
                                arms.push(a);
                                self.eat(",");
                            }
                            Err(()) => self.recover_stmt(),
                        }
                        self.stmt_depth.pop();
                    }
                    self.expect("}")?;
                    return Ok(StmtKind::Match { scrutinee, arms });
                }
                "for" => {
                    self.bump();
                    let var = self.ident("a loop variable")?;
                    if !self.eat_kw("in") {
                        self.unexpected("`in`");
                        return Err(());
                    }
                    let start = self.expr_bp(BP_ADD)?;
                    let inclusive = if self.eat("..") {
                        false
                    } else if self.eat("..=") {
                        true
                    } else {
                        let sp = self.span();
                        self.error(
                            Diagnostic::error(
                                sp,
                                format!("expected `..` or `..=`, found {}", self.peek().describe()),
                            )
                            .with_label("expected a range")
                            .with_help("loops run over constant ranges: `for i in 0..8 { ... }`"),
                        );
                        return Err(());
                    };
                    let end = self.cond_expr()?;
                    let body = self.block()?;
                    return Ok(StmtKind::For { var, start, end, inclusive, body });
                }
                "assert" => {
                    self.bump();
                    return Ok(StmtKind::Assert(self.assert_rest()?));
                }
                "always" | "always_ff" | "always_comb" | "initial" | "begin" | "end" | "assign" => {
                    let sp = self.span();
                    let help = match k.as_str() {
                        "assign" => "drive a signal directly: `y = a & b;`",
                        "initial" => {
                            "give power-on values in declarations: `reg r: uint<8> init 0;` or `mem m: [...] init ...;`"
                        }
                        "begin" | "end" => "blocks are written with `{` and `}`",
                        _ => {
                            "declare registers with `reg` and set their next value with `:=`; combinational signals are assigned with `=`; there are no process blocks"
                        }
                    };
                    self.error(
                        Diagnostic::error(sp, format!("`{k}` is Verilog, not Weft"))
                            .with_label("not a Weft statement")
                            .with_help(help),
                    );
                    return Err(());
                }
                _ => {}
            }
        }
        if self.is("{") {
            return Ok(StmtKind::Block(self.block()?));
        }
        // Assignment.
        let target = self.expr_bp(BP_CMP + 1)?;
        let op = if self.eat("=") {
            AssignOp::Drive
        } else if self.eat(":=") {
            AssignOp::Next
        } else if self.is("<=") {
            let sp = self.span();
            self.error(
                Diagnostic::error(sp, "`<=` is a comparison, not an assignment")
                    .with_label("Verilog non-blocking assignment?")
                    .with_help("set a register's next value with `:=`: `count := count +% 1;`")
                    .with_help("drive a combinational signal with `=`"),
            );
            return Err(());
        } else if self.is("==") {
            let sp = self.span();
            self.error(
                Diagnostic::error(sp, "a comparison is not a statement")
                    .with_label("did you mean `=`?")
                    .with_help("assign with `=` (combinational) or `:=` (register next value)"),
            );
            return Err(());
        } else {
            let sp = self.span();
            let found = self.peek().describe();
            self.error(
                Diagnostic::error(sp, format!("expected `=` or `:=`, found {found}"))
                    .with_label("expected an assignment")
                    .with_secondary(target.span, "this is not a statement on its own"),
            );
            return Err(());
        };
        let value = self.expr()?;
        self.expect(";")?;
        Ok(StmtKind::Assign { target, op, value })
    }

    fn decl_colon(&mut self, name: &Ident, kw: &str) -> R<()> {
        if self.is(":") {
            self.bump();
            return Ok(());
        }
        let sp = self.span();
        self.error(
            Diagnostic::error(sp, format!("expected `:` and a type after `{kw} {}`", name.name))
                .with_label("expected `:`")
                .with_help(format!("write `{kw} {}: uint<8> ...`", name.name)),
        );
        Err(())
    }

    fn arm_stmt(&mut self) -> R<Stmt> {
        if self.is("{") || self.is_kw("if") || self.is_kw("match") {
            return self.stmt();
        }
        let start = self.span();
        let target = self.expr_bp(BP_CMP + 1)?;
        let op = if self.eat("=") {
            AssignOp::Drive
        } else if self.eat(":=") {
            AssignOp::Next
        } else {
            let sp = self.span();
            self.error(
                Diagnostic::error(sp, format!("expected `=` or `:=`, found {}", self.peek().describe()))
                    .with_label("expected an assignment")
                    .with_help("in a `match` statement each arm is an assignment, an `if`, a `match` or a `{ block }`; for a value, use a `match` expression: `x = match s { ... };`"),
            );
            return Err(());
        };
        let value = self.expr()?;
        self.expect(";")?;
        Ok(Stmt {
            kind: StmtKind::Assign { target, op, value },
            attrs: vec![],
            span: start.to(self.prev_span()),
        })
    }

    fn pattern(&mut self) -> R<Pattern> {
        let first = self.pat_atom()?;
        if !self.is("|") {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.eat("|") {
            alts.push(self.pat_atom()?);
        }
        let span = alts[0].span.to(alts.last().unwrap().span);
        Ok(Pattern { kind: PatKind::Or(alts), span })
    }

    fn pat_int(&mut self) -> R<BigInt> {
        let neg = self.eat("-");
        match self.peek().clone() {
            Tok::Int(v, _) => {
                self.bump();
                Ok(if neg { -&v } else { v })
            }
            _ => {
                self.unexpected("a number");
                Err(())
            }
        }
    }

    fn pat_atom(&mut self) -> R<Pattern> {
        let start = self.span();
        let kind = match self.peek().clone() {
            Tok::Int(..) | Tok::Punct("-") => {
                let lo = self.pat_int()?;
                if self.eat("..=") {
                    let hi = self.pat_int()?;
                    PatKind::Range(lo, hi)
                } else if self.is("..") {
                    let sp = self.span();
                    self.error(
                        Diagnostic::error(sp, "range patterns are inclusive")
                            .with_label("use `..=`")
                            .with_help("write `0..=9` for the values 0 to 9"),
                    );
                    return Err(());
                } else {
                    PatKind::Int(lo)
                }
            }
            Tok::Bits(v, c, w) => {
                self.bump();
                PatKind::Bits(v, c, w)
            }
            Tok::Punct(".") => {
                self.bump();
                PatKind::Variant(None, self.ident("a variant name")?)
            }
            Tok::Ident(k) if k == "true" || k == "false" => {
                self.bump();
                PatKind::Bool(k == "true")
            }
            Tok::Ident(k) if k == "_" => {
                self.bump();
                PatKind::Wild
            }
            Tok::Ident(k) if k == "default" => {
                let sp = self.span();
                self.error(
                    Diagnostic::error(sp, "`default` is not a pattern")
                        .with_label("Verilog `default:`?")
                        .with_help("write `_ => ...` for the remaining values"),
                );
                return Err(());
            }
            Tok::Ident(_) => {
                let ty = self.ident("an enum name")?;
                if !self.eat(".") {
                    self.error(
                        Diagnostic::error(ty.span, "a name alone is not a pattern")
                            .with_label("expected a literal, a range, `.Variant` or `_`")
                            .with_help(format!("for an enum variant, write `.{}`; to compare with a constant, use its value or an `if`", ty.name)),
                    );
                    return Err(());
                }
                let v = self.ident("a variant name")?;
                PatKind::Variant(Some(ty), v)
            }
            _ => {
                self.unexpected("a pattern");
                return Err(());
            }
        };
        Ok(Pattern { kind, span: start.to(self.prev_span()) })
    }

    fn gen_arg_value(&mut self) -> R<GenArgValue> {
        // Try a type first; fall back to an expression.
        let save = self.pos;
        let nd = self.diags.items.len();
        if let Ok(t) = self.ty()
            && (self.is(",") || self.is_close_angle())
        {
            return Ok(GenArgValue::Type(t));
        }
        self.diags.items.truncate(nd);
        self.pos = save;
        Ok(GenArgValue::Expr(self.expr_bp(BP_SHIFT + 1)?))
    }

    /// An expression followed by a block (`if`, `match`, `for` headers).
    fn cond_expr(&mut self) -> R<Expr> {
        let e = self.expr()?;
        if self.is("=") && !self.is_at(1, "=") {
            let sp = self.span();
            self.error(
                Diagnostic::error(sp, "expected `{`, found `=`")
                    .with_label("assignment in a condition?")
                    .with_help("compare with `==`"),
            );
            return Err(());
        }
        Ok(e)
    }

    pub fn expr(&mut self) -> R<Expr> {
        self.expr_bp(0)
    }

    fn expr_bp(&mut self, min_bp: u8) -> R<Expr> {
        let mut lhs = self.unary()?;
        while let Tok::Punct(p) = self.peek().clone() {
            if p == "?" {
                let sp = self.span();
                self.error(
                    Diagnostic::error(sp, "Weft has no `?:` operator")
                        .with_label("conditional operator")
                        .with_help("use an `if` expression: `if c { a } else { b }`"),
                );
                return Err(());
            }
            let Some((op, bp, right_assoc)) = binop(p) else { break };
            if bp < min_bp {
                break;
            }
            let op_span = self.bump().span;
            let rhs = self.expr_bp(if right_assoc { bp } else { bp + 1 })?;
            self.check_mixing(op, op_span, &lhs, &rhs)?;
            let span = lhs.span.to(rhs.span);
            lhs = Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span };
        }
        Ok(lhs)
    }

    fn check_mixing(&mut self, op: BinOp, op_span: Span, lhs: &Expr, rhs: &Expr) -> R<()> {
        use BinOp::*;
        let child = |e: &Expr| match &e.kind {
            ExprKind::Binary(o, ..) => Some(*o),
            _ => None,
        };
        for side in [lhs, rhs] {
            let Some(c) = child(side) else { continue };
            let bad = match (op, c) {
                (LogicOr, LogicAnd) | (LogicAnd, LogicOr) => true,
                (And | Or | Xor, And | Or | Xor) => op != c,
                _ => false,
            };
            if bad {
                self.error(
                    Diagnostic::error(
                        op_span,
                        format!(
                            "`{}` and `{}` are mixed without parentheses",
                            c.symbol(),
                            op.symbol()
                        ),
                    )
                    .with_label("ambiguous to a reader")
                    .with_secondary(side.span, "this part needs parentheses")
                    .with_help(format!(
                        "group explicitly, for example `(a {} b) {} c`",
                        c.symbol(),
                        op.symbol()
                    )),
                );
                return Err(());
            }
            if op.is_compare() && c.is_compare() && std::ptr::eq(side, lhs) {
                self.error(
                    Diagnostic::error(op_span, "comparisons cannot be chained")
                        .with_label("second comparison")
                        .with_secondary(side.span, "first comparison")
                        .with_help("combine them with `&&`: `a < b && b < c`"),
                );
                return Err(());
            }
        }
        Ok(())
    }

    fn unary(&mut self) -> R<Expr> {
        let start = self.span();
        let op = if self.is("-") {
            Some(UnOp::Neg)
        } else if self.is("!") {
            Some(UnOp::Not)
        } else if self.is("~") {
            Some(UnOp::BitNot)
        } else {
            None
        };
        if let Some(op) = op {
            self.bump();
            let e = self.expr_bp(BP_UNARY)?;
            let span = start.to(e.span);
            return Ok(Expr { kind: ExprKind::Unary(op, Box::new(e)), span });
        }
        if self.is("&") || self.is("|") || self.is("^") {
            let sp = self.span();
            let (m, d) = match self.peek() {
                Tok::Punct("&") => ("all", "all bits set"),
                Tok::Punct("|") => ("any", "any bit set"),
                _ => ("parity", "XOR of all bits"),
            };
            self.error(
                Diagnostic::error(sp, "Weft has no reduction operators")
                    .with_label("Verilog reduction?")
                    .with_help(format!("use the method `x.{m}()` ({d})")),
            );
            return Err(());
        }
        self.postfix()
    }

    fn postfix(&mut self) -> R<Expr> {
        let mut e = self.primary()?;
        loop {
            if self.is(".") {
                self.bump();
                let name = self.field_name("a field or method name")?;
                if self.is("(") {
                    let args = self.args()?;
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::Method(Box::new(e), name, args), span };
                } else {
                    let span = e.span.to(name.span);
                    e = Expr { kind: ExprKind::Field(Box::new(e), name), span };
                }
            } else if self.is("[") {
                self.bump();
                let a = self.expr()?;
                if self.eat(":") {
                    let b = self.expr()?;
                    self.expect("]")?;
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::Slice(Box::new(e), Box::new(a), Box::new(b)), span };
                } else if self.eat("+:") {
                    let b = self.expr()?;
                    self.expect("]")?;
                    let span = e.span.to(self.prev_span());
                    e = Expr {
                        kind: ExprKind::PartSelect(Box::new(e), Box::new(a), Box::new(b)),
                        span,
                    };
                } else {
                    self.expect("]")?;
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::Index(Box::new(e), Box::new(a)), span };
                }
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn args(&mut self) -> R<Vec<Arg>> {
        self.expect("(")?;
        let mut out = Vec::new();
        while !self.is(")") {
            let name = if matches!(self.peek(), Tok::Ident(_)) && self.is_at(1, ":") {
                Some(self.ident("an argument name")?)
            } else {
                None
            };
            if name.is_some() {
                self.bump();
            }
            let value = self.expr()?;
            out.push(Arg { name, value });
            if !self.eat(",") {
                break;
            }
        }
        self.expect(")")?;
        Ok(out)
    }

    fn expr_block(&mut self) -> R<ExprBlock> {
        let open = self.expect("{")?;
        let mut lets = Vec::new();
        while self.is_kw("let") {
            lets.push(self.stmt()?);
        }
        if self.is("}") {
            let sp = self.span();
            self.error(
                Diagnostic::error(sp, "expected a value")
                    .with_label("empty branch")
                    .with_secondary(open, "branch starts here"),
            );
            return Err(());
        }
        let value = self.expr()?;
        if self.is(";") {
            let sp = self.span();
            self.error(
                Diagnostic::error(sp, "unexpected `;` in an expression branch")
                    .with_label("remove this `;`")
                    .with_help("the branches of an `if` or `match` expression are values; to assign signals conditionally, use an `if` statement"),
            );
            return Err(());
        }
        self.expect("}")?;
        Ok(ExprBlock { lets, value: Box::new(value) })
    }

    fn primary(&mut self) -> R<Expr> {
        let start = self.span();
        let tok = self.peek().clone();
        let kind = match tok {
            Tok::Int(v, w) => {
                self.bump();
                ExprKind::Int(v, w)
            }
            Tok::Bits(..) => {
                let sp = self.span();
                self.error(
                    Diagnostic::error(sp, "`?` digits are only allowed in `match` patterns")
                        .with_label("bit pattern outside a pattern"),
                );
                self.bump();
                return Err(());
            }
            Tok::Str(s) => {
                self.bump();
                ExprKind::Str(s)
            }
            Tok::Punct("(") => {
                self.bump();
                let e = self.expr()?;
                self.expect(")")?;
                ExprKind::Paren(Box::new(e))
            }
            Tok::Punct(".") => {
                self.bump();
                ExprKind::Variant(self.ident("a variant name")?)
            }
            Tok::Punct("[") => {
                self.bump();
                if self.eat("]") {
                    let sp = start.to(self.prev_span());
                    self.error(
                        Diagnostic::error(sp, "arrays cannot be empty").with_label("empty array"),
                    );
                    return Err(());
                }
                let first = self.expr()?;
                if self.eat(";") {
                    let n = self.expr()?;
                    self.expect("]")?;
                    ExprKind::ArrayRepeat(Box::new(first), Box::new(n))
                } else {
                    let mut items = vec![first];
                    while self.eat(",") {
                        if self.is("]") {
                            break;
                        }
                        items.push(self.expr()?);
                    }
                    self.expect("]")?;
                    ExprKind::Array(items)
                }
            }
            Tok::Punct("{") => {
                let sp = self.span();
                self.error(
                    Diagnostic::error(sp, "expected an expression, found `{`")
                        .with_label("Verilog concatenation?")
                        .with_help("concatenate with `concat(a, b, ...)`, most significant first"),
                );
                return Err(());
            }
            Tok::Ident(k) => {
                match k.as_str() {
                    "true" | "false" => {
                        self.bump();
                        ExprKind::Bool(k == "true")
                    }
                    "undef" => {
                        self.bump();
                        ExprKind::Undef
                    }
                    "default" => {
                        self.bump();
                        ExprKind::Default
                    }
                    "if" => {
                        self.bump();
                        let cond = self.cond_expr()?;
                        let then = self.expr_block()?;
                        if !self.is_kw("else") {
                            let sp = self.span();
                            self.error(
                            Diagnostic::error(sp, "an `if` expression needs an `else` branch")
                                .with_label("expected `else`")
                                .with_help("give the value for the other case: `if c { a } else { b }`"),
                        );
                            return Err(());
                        }
                        self.bump();
                        let els = if self.is_kw("if") {
                            self.primary()?
                        } else {
                            let s = self.span();
                            let b = self.expr_block()?;
                            Expr { kind: ExprKind::Block(b), span: s.to(self.prev_span()) }
                        };
                        ExprKind::If(Box::new(cond), then, Box::new(els))
                    }
                    "match" => {
                        self.bump();
                        let scr = self.cond_expr()?;
                        self.expect("{")?;
                        let mut arms = Vec::new();
                        while !self.is("}") {
                            let p = self.pattern()?;
                            self.expect("=>")?;
                            let v = self.expr()?;
                            arms.push((p, v));
                            if !self.eat(",") {
                                break;
                            }
                        }
                        self.expect("}")?;
                        ExprKind::Match(Box::new(scr), arms)
                    }
                    "uint" | "sint" | "index" => {
                        let ty = self.ty()?;
                        if !self.is("(") {
                            let sp = self.span();
                            self.error(
                                Diagnostic::error(sp, "a type cannot be used as a value")
                                    .with_label("expected `(`")
                                    .with_help(format!(
                                        "to convert a value, write `{}(x)`",
                                        self.snippet_ty(&ty)
                                    )),
                            );
                            return Err(());
                        }
                        self.bump();
                        let e = self.expr()?;
                        self.expect(")")?;
                        ExprKind::Convert(ty, Box::new(e))
                    }
                    _ => {
                        let id = self.ident("an expression")?;
                        if let Some(generics) = self.generic_call_args() {
                            let args = self.args()?;
                            let dom = self.dom_opt()?;
                            ExprKind::Call { callee: id, generics, args, dom }
                        } else if self.is("(") {
                            let args = self.args()?;
                            let dom = self.dom_opt()?;
                            ExprKind::Call { callee: id, generics: vec![], args, dom }
                        } else if self.is("{")
                            && matches!(self.peek_at(1), Tok::Ident(_))
                            && self.is_at(2, ":")
                            && id.name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                        {
                            self.bump();
                            let mut fields = Vec::new();
                            while !self.is("}") {
                                let f = self.ident("a field name")?;
                                self.expect(":")?;
                                let v = self.expr()?;
                                fields.push((f, v));
                                if !self.eat(",") {
                                    break;
                                }
                            }
                            self.expect("}")?;
                            ExprKind::Struct(id, fields)
                        } else {
                            ExprKind::Ident(id)
                        }
                    }
                }
            }
            _ => {
                self.unexpected("an expression");
                return Err(());
            }
        };
        Ok(Expr { kind, span: start.to(self.prev_span()) })
    }

    fn snippet_ty(&self, t: &TypeExpr) -> String {
        match &t.kind {
            TypeKind::Uint(_) => "uint<N>".into(),
            TypeKind::Sint(_) => "sint<N>".into(),
            TypeKind::Index(_) => "index<N>".into(),
            _ => "T".into(),
        }
    }

    /// `<...>` followed by `(` after a name: generic arguments of a call.
    /// Anything else (a comparison) leaves the position unchanged.
    fn generic_call_args(&mut self) -> Option<Vec<GenArg>> {
        if !self.is("<") {
            return None;
        }
        let named = matches!(self.peek_at(1), Tok::Ident(_)) && self.is_at(2, ":");
        let (save, nd) = (self.pos, self.diags.items.len());
        match self.gen_args() {
            Ok(g) if self.is("(") => Some(g),
            _ if named => {
                // Clearly meant as generic arguments: keep the errors.
                if self.diags.items.len() == nd {
                    self.unexpected("`(`");
                }
                Some(Vec::new())
            }
            _ => {
                self.diags.items.truncate(nd);
                self.pos = save;
                None
            }
        }
    }

    fn gen_args(&mut self) -> R<Vec<GenArg>> {
        self.expect("<")?;
        let mut out = Vec::new();
        while !self.is_close_angle() {
            // `NAME: value`, or a bare value for a single parameter.
            let named =
                matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Punct(":"));
            let name = if named {
                let n = self.ident("a generic parameter name")?;
                self.expect(":")?;
                Some(n)
            } else {
                None
            };
            let value = self.gen_arg_value()?;
            out.push(GenArg { name, value });
            if !self.eat(",") {
                break;
            }
        }
        self.expect(">")?;
        Ok(out)
    }
}

const BP_CMP: u8 = 3;
const BP_SHIFT: u8 = 7;
const BP_ADD: u8 = 8;
const BP_UNARY: u8 = 10;

/// Binding power and associativity of a binary operator token.
fn binop(p: &str) -> Option<(BinOp, u8, bool)> {
    use BinOp::*;
    Some(match p {
        "||" => (LogicOr, 1, false),
        "&&" => (LogicAnd, 2, false),
        "==" => (Eq, BP_CMP, false),
        "!=" => (Ne, BP_CMP, false),
        "<" => (Lt, BP_CMP, false),
        "<=" => (Le, BP_CMP, false),
        ">" => (Gt, BP_CMP, false),
        ">=" => (Ge, BP_CMP, false),
        "|" => (Or, 4, false),
        "^" => (Xor, 5, false),
        "&" => (And, 6, false),
        "<<" => (Shl, BP_SHIFT, false),
        ">>" => (Shr, BP_SHIFT, false),
        "+" => (Add, BP_ADD, false),
        "-" => (Sub, BP_ADD, false),
        "+%" => (AddWrap, BP_ADD, false),
        "-%" => (SubWrap, BP_ADD, false),
        "*" => (Mul, 9, false),
        "*%" => (MulWrap, 9, false),
        "/" => (Div, 9, false),
        "%" => (Rem, 9, false),
        "**" => (Pow, 11, true),
        _ => return None,
    })
}
