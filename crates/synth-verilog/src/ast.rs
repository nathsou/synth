//! Syntax tree for synthesizable Verilog-2005.

use synth_core::Span;

/// An identifier with its location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// `(* name = value, ... *)`
#[derive(Clone, Debug, PartialEq)]
pub struct Attribute {
    pub name: Ident,
    pub value: Option<Expr>,
}

pub type Attrs = Vec<Attribute>;

#[derive(Clone, Debug, PartialEq)]
pub struct SourceText {
    pub modules: Vec<Module>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub name: Ident,
    pub attrs: Attrs,
    /// Parameters from the `#( ... )` header.
    pub params: Vec<ParamDecl>,
    pub ports: Ports,
    pub items: Vec<Item>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ports {
    /// `module m(a, b);` with directions declared in the body.
    NonAnsi(Vec<Ident>),
    /// `module m(input a, output [3:0] b);`
    Ansi(Vec<PortDecl>),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
    Inout,
}

/// Kind of a data object.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NetKind {
    Wire,
    Tri,
    Wand,
    Wor,
    Supply0,
    Supply1,
    Reg,
    Integer,
    Time,
    Real,
    /// SystemVerilog `logic` (accepted as a convenience).
    Logic,
}

impl NetKind {
    /// True for variables (assigned in procedures) rather than nets.
    pub fn is_variable(self) -> bool {
        matches!(
            self,
            NetKind::Reg | NetKind::Integer | NetKind::Time | NetKind::Real | NetKind::Logic
        )
    }
}

/// `[msb:lsb]`
#[derive(Clone, Debug, PartialEq)]
pub struct Range {
    pub msb: Expr,
    pub lsb: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PortDecl {
    pub attrs: Attrs,
    pub dir: Direction,
    /// Explicit kind (`input wire`, `output reg`); `None` means default net.
    pub kind: Option<NetKind>,
    pub signed: bool,
    pub range: Option<Range>,
    /// Names, each with an optional initializer (`output reg q = 0`).
    pub names: Vec<(Ident, Option<Expr>)>,
    pub span: Span,
}

/// A declared name with unpacked dimensions and an optional initializer.
#[derive(Clone, Debug, PartialEq)]
pub struct DeclName {
    pub name: Ident,
    pub dims: Vec<Range>,
    pub init: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VarDecl {
    pub attrs: Attrs,
    pub kind: NetKind,
    pub signed: bool,
    pub range: Option<Range>,
    pub names: Vec<DeclName>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParamDecl {
    pub local: bool,
    pub signed: bool,
    pub range: Option<Range>,
    /// `parameter integer P = ...` and similar type keywords.
    pub kind: Option<NetKind>,
    pub assigns: Vec<(Ident, Expr)>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParamOverride {
    Ordered(Vec<Expr>),
    Named(Vec<(Ident, Option<Expr>)>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PortConn {
    Ordered(Option<Expr>),
    Named(Ident, Option<Expr>),
    /// `.*` (SystemVerilog) is not supported; kept for diagnostics.
    Wildcard(Span),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Instance {
    pub name: Ident,
    /// Instance array range: `inst [3:0] (...)`.
    pub range: Option<Range>,
    pub conns: Vec<PortConn>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstanceDecl {
    pub attrs: Attrs,
    pub module: Ident,
    pub params: Option<ParamOverride>,
    pub instances: Vec<Instance>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenBlock {
    pub name: Option<Ident>,
    pub items: Vec<Item>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenCaseItem {
    /// Empty for `default`.
    pub exprs: Vec<Expr>,
    pub body: GenBlock,
}

/// A function or task argument/local declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct SubroutineDecl {
    pub automatic: bool,
    pub name: Ident,
    /// Return type of functions.
    pub signed: bool,
    pub range: Option<Range>,
    pub ret_kind: Option<NetKind>,
    pub ports: Vec<PortDecl>,
    pub decls: Vec<Item>,
    pub body: Stmt,
    pub span: Span,
}

/// Built-in gate primitives (`and`, `or`, `not`, `buf`, ...).
#[derive(Clone, Debug, PartialEq)]
pub struct GateInstance {
    pub kind: String,
    pub name: Option<Ident>,
    pub conns: Vec<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Port(PortDecl),
    Var(VarDecl),
    Param(ParamDecl),
    Genvar(Vec<Ident>),
    /// `assign a = b, c = d;`
    ContAssign {
        attrs: Attrs,
        assigns: Vec<(Expr, Expr)>,
        span: Span,
    },
    Always {
        attrs: Attrs,
        body: Stmt,
        span: Span,
    },
    Initial {
        body: Stmt,
        span: Span,
    },
    Instance(InstanceDecl),
    Gate(GateInstance),
    GenFor {
        init: (Ident, Expr),
        cond: Expr,
        step: (Ident, Expr),
        body: GenBlock,
        span: Span,
    },
    GenIf {
        cond: Expr,
        then: GenBlock,
        otherwise: Option<GenBlock>,
        span: Span,
    },
    GenCase {
        expr: Expr,
        items: Vec<GenCaseItem>,
        span: Span,
    },
    /// A named or unnamed `begin ... end` block in a generate region.
    GenBlock(GenBlock),
    Function(SubroutineDecl),
    Task(SubroutineDecl),
    Defparam(Vec<(Vec<Ident>, Expr)>, Span),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CaseKind {
    Case,
    Casez,
    Casex,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaseItem {
    /// Empty for `default`.
    pub exprs: Vec<Expr>,
    pub body: Stmt,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Edge {
    Posedge,
    Negedge,
    Any,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventExpr {
    pub edge: Edge,
    pub expr: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EventControl {
    /// `@*` or `@(*)`.
    Star,
    List(Vec<EventExpr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Null(Span),
    Block {
        name: Option<Ident>,
        decls: Vec<Item>,
        stmts: Vec<Stmt>,
        span: Span,
    },
    Assign {
        lhs: Expr,
        rhs: Expr,
        nonblocking: bool,
        span: Span,
    },
    If {
        cond: Expr,
        then: Box<Stmt>,
        otherwise: Option<Box<Stmt>>,
        attrs: Attrs,
        span: Span,
    },
    Case {
        kind: CaseKind,
        expr: Expr,
        items: Vec<CaseItem>,
        attrs: Attrs,
        span: Span,
    },
    For {
        init: Box<Stmt>,
        cond: Expr,
        step: Box<Stmt>,
        body: Box<Stmt>,
        span: Span,
    },
    While {
        cond: Expr,
        body: Box<Stmt>,
        span: Span,
    },
    Repeat {
        count: Expr,
        body: Box<Stmt>,
        span: Span,
    },
    Forever {
        body: Box<Stmt>,
        span: Span,
    },
    Timing {
        control: EventControl,
        body: Box<Stmt>,
        span: Span,
    },
    /// `#delay stmt` (delays are ignored by synthesis).
    Delay {
        body: Box<Stmt>,
        span: Span,
    },
    /// User task call.
    TaskCall {
        name: Ident,
        args: Vec<Expr>,
        span: Span,
    },
    /// System task call (`$display`, `$readmemh`, ...).
    SysTask {
        name: Ident,
        args: Vec<Expr>,
        span: Span,
    },
    Disable {
        name: Ident,
        span: Span,
    },
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Null(s) => *s,
            Stmt::Block { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::If { span, .. }
            | Stmt::Case { span, .. }
            | Stmt::For { span, .. }
            | Stmt::While { span, .. }
            | Stmt::Repeat { span, .. }
            | Stmt::Forever { span, .. }
            | Stmt::Timing { span, .. }
            | Stmt::Delay { span, .. }
            | Stmt::TaskCall { span, .. }
            | Stmt::SysTask { span, .. }
            | Stmt::Disable { span, .. } => *span,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    LogicNot,
    BitNot,
    RedAnd,
    RedNand,
    RedOr,
    RedNor,
    RedXor,
    RedXnor,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    Ne,
    CaseEq,
    CaseNe,
    Lt,
    Le,
    Gt,
    Ge,
    LogicAnd,
    LogicOr,
    BitAnd,
    BitOr,
    BitXor,
    BitXnor,
    Shl,
    Shr,
    AShl,
    AShr,
}

/// A number literal.
#[derive(Clone, Debug, PartialEq)]
pub struct Number {
    /// Explicit size, if any.
    pub size: Option<u32>,
    pub signed: bool,
    /// `b`, `o`, `d`, `h`, or `u` for SystemVerilog `'0`/`'1`/`'x`/`'z`;
    /// `None` for a plain decimal number.
    pub base: Option<char>,
    pub digits: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Number(Number, Span),
    Real(String, Span),
    Str(String, Span),
    /// A (possibly hierarchical) name: `a`, `blk.a`, `gen[2].x`.
    Ident(Vec<(Ident, Option<Box<Expr>>)>, Span),
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    /// `base[msb:lsb]`
    Range {
        base: Box<Expr>,
        msb: Box<Expr>,
        lsb: Box<Expr>,
        span: Span,
    },
    /// `base[start +: width]` (`up`) or `base[start -: width]`
    IndexedRange {
        base: Box<Expr>,
        start: Box<Expr>,
        width: Box<Expr>,
        up: bool,
        span: Span,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
        span: Span,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    Ternary {
        cond: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
        span: Span,
    },
    Concat(Vec<Expr>, Span),
    Repeat {
        count: Box<Expr>,
        exprs: Vec<Expr>,
        span: Span,
    },
    Call {
        name: Ident,
        args: Vec<Expr>,
        span: Span,
    },
    SysCall {
        name: Ident,
        args: Vec<Expr>,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Number(_, s)
            | Expr::Real(_, s)
            | Expr::Str(_, s)
            | Expr::Ident(_, s)
            | Expr::Concat(_, s) => *s,
            Expr::Index { span, .. }
            | Expr::Range { span, .. }
            | Expr::IndexedRange { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Ternary { span, .. }
            | Expr::Repeat { span, .. }
            | Expr::Call { span, .. }
            | Expr::SysCall { span, .. } => *span,
        }
    }

    /// The simple name of an unindexed, non-hierarchical identifier.
    pub fn as_simple_ident(&self) -> Option<&Ident> {
        match self {
            Expr::Ident(parts, _) if parts.len() == 1 && parts[0].1.is_none() => Some(&parts[0].0),
            _ => None,
        }
    }
}
