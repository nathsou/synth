//! Syntax tree of Weft sources.

use synth_core::{BigInt, Span};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub struct File {
    pub items: Vec<Item>,
}

#[derive(Clone, Debug)]
pub struct Attribute {
    pub name: Ident,
    pub args: Vec<Expr>,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: ItemKind,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ItemKind {
    Module(Module),
    Function(Function),
    Struct(StructDef),
    Enum(EnumDef),
    Const(ConstDecl),
    Assert(Assert),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GenericKind {
    Int,
    Bool,
    Str,
    Type,
}

#[derive(Clone, Debug)]
pub struct Generic {
    pub name: Ident,
    pub kind: GenericKind,
    pub default: Option<GenArgValue>,
}

#[derive(Clone, Debug)]
pub struct Module {
    pub name: Ident,
    pub generics: Vec<Generic>,
    pub ports: Vec<Port>,
    /// `None` for extern modules.
    pub body: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum DomRefKind {
    Named(Ident),
    Async,
    Default,
}

#[derive(Clone, Debug)]
pub struct DomRef {
    pub kind: DomRefKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum PortKind {
    In {
        ty: TypeExpr,
        dom: Option<DomRef>,
        default: Option<Expr>,
    },
    Out {
        ty: TypeExpr,
        dom: Option<DomRef>,
    },
    Inout {
        ty: TypeExpr,
    },
    Domain {
        default: bool,
    },
    /// Extern modules: a domain bound to Verilog clock and reset ports.
    Binding {
        clock: Ident,
        reset: Option<(Ident, bool)>,
    },
}

#[derive(Clone, Debug)]
pub struct Port {
    /// For an unnamed binding, `default`.
    pub name: Ident,
    pub kind: PortKind,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub name: Ident,
    pub generics: Vec<Generic>,
    pub params: Vec<(Ident, TypeExpr)>,
    pub ret: TypeExpr,
    pub body: Vec<Stmt>,
    pub result: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct StructDef {
    pub name: Ident,
    pub fields: Vec<(Ident, TypeExpr)>,
}

#[derive(Clone, Debug)]
pub struct EnumDef {
    pub name: Ident,
    pub repr: Option<TypeExpr>,
    pub variants: Vec<(Ident, Option<Expr>)>,
}

#[derive(Clone, Debug)]
pub struct ConstDecl {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    /// A compile-time type annotation (`int`, `string`), instead of `ty`.
    pub kind: Option<(GenericKind, Span)>,
    pub value: Expr,
}

#[derive(Clone, Debug)]
pub struct Assert {
    pub cond: Expr,
    pub message: String,
}

#[derive(Clone, Debug)]
pub enum TypeKind {
    Bool,
    Clock,
    Uint(Box<Expr>),
    Sint(Box<Expr>),
    Index(Box<Expr>),
    Array(Box<TypeExpr>, Box<Expr>),
    Named(Ident),
}

#[derive(Clone, Debug)]
pub struct TypeExpr {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum GenArgValue {
    Type(TypeExpr),
    Expr(Expr),
}

#[derive(Clone, Debug)]
pub struct GenArg {
    /// `None` for a positional argument (only for a single parameter).
    pub name: Option<Ident>,
    pub value: GenArgValue,
}

impl GenArgValue {
    pub fn span(&self) -> Span {
        match self {
            GenArgValue::Type(t) => t.span,
            GenArgValue::Expr(e) => e.span,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Arg {
    pub name: Option<Ident>,
    pub value: Expr,
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AssignOp {
    /// `=`
    Drive,
    /// `:=`
    Next,
}

#[derive(Clone, Debug)]
pub enum DomFieldValue {
    Expr(Expr),
    /// `edge: rising` (true) or `edge: falling` (false).
    Edge(bool),
}

#[derive(Clone, Debug)]
pub struct DomField {
    pub name: Ident,
    pub value: DomFieldValue,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    Const(ConstDecl),
    Let { name: Ident, ty: Option<TypeExpr>, dom: Option<DomRef>, value: Expr },
    Var { name: Ident, ty: Option<TypeExpr>, value: Expr },
    Wire { name: Ident, ty: TypeExpr, dom: Option<DomRef> },
    Reg { name: Ident, ty: TypeExpr, dom: Option<DomRef>, value: Expr, init_only: bool },
    Mem { name: Ident, elem: TypeExpr, depth: Expr, dom: Option<DomRef>, init: Option<Expr> },
    Inst { name: Ident, call: Expr },
    Domain { name: Ident, default: bool, fields: Vec<DomField> },
    Assign { target: Expr, op: AssignOp, value: Expr },
    If { cond: Expr, then: Vec<Stmt>, els: Option<Vec<Stmt>>, else_span: Option<Span> },
    Match { scrutinee: Expr, arms: Vec<(Pattern, Stmt)> },
    For { var: Ident, start: Expr, end: Expr, inclusive: bool, body: Vec<Stmt> },
    Assert(Assert),
    Block(Vec<Stmt>),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    AddWrap,
    SubWrap,
    MulWrap,
    Pow,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LogicAnd,
    LogicOr,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        use BinOp::*;
        match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Rem => "%",
            AddWrap => "+%",
            SubWrap => "-%",
            MulWrap => "*%",
            Pow => "**",
            And => "&",
            Or => "|",
            Xor => "^",
            Shl => "<<",
            Shr => ">>",
            Eq => "==",
            Ne => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            LogicAnd => "&&",
            LogicOr => "||",
        }
    }

    pub fn is_compare(self) -> bool {
        use BinOp::*;
        matches!(self, Eq | Ne | Lt | Le | Gt | Ge)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
}

/// A block in expression position: `{ let a = ...; value }`.
#[derive(Clone, Debug)]
pub struct ExprBlock {
    pub lets: Vec<Stmt>,
    pub value: Box<Expr>,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(BigInt, Option<u32>),
    Bool(bool),
    Str(String),
    Undef,
    Ident(Ident),
    /// The `default` domain, in `default.clock`.
    Default,
    /// `.Name`: an enum variant of the expected type.
    Variant(Ident),
    Paren(Box<Expr>),
    Field(Box<Expr>, Ident),
    Method(Box<Expr>, Ident, Vec<Arg>),
    Index(Box<Expr>, Box<Expr>),
    Slice(Box<Expr>, Box<Expr>, Box<Expr>),
    PartSelect(Box<Expr>, Box<Expr>, Box<Expr>),
    Call {
        callee: Ident,
        generics: Vec<GenArg>,
        args: Vec<Arg>,
        dom: Option<DomRef>,
    },
    Convert(TypeExpr, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    If(Box<Expr>, ExprBlock, Box<Expr>),
    Block(ExprBlock),
    Match(Box<Expr>, Vec<(Pattern, Expr)>),
    Struct(Ident, Vec<(Ident, Expr)>),
    Array(Vec<Expr>),
    ArrayRepeat(Box<Expr>, Box<Expr>),
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    /// The expression without enclosing parentheses.
    pub fn strip(&self) -> &Expr {
        match &self.kind {
            ExprKind::Paren(e) => e.strip(),
            _ => self,
        }
    }
}

#[derive(Clone, Debug)]
pub enum PatKind {
    Int(BigInt),
    Range(BigInt, BigInt),
    /// (value, care mask, width)
    Bits(u128, u128, u32),
    Variant(Option<Ident>, Ident),
    Bool(bool),
    Wild,
    Or(Vec<Pattern>),
}

#[derive(Clone, Debug)]
pub struct Pattern {
    pub kind: PatKind,
    pub span: Span,
}
