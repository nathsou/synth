//! Document symbols (the outline), from the syntax tree.

use synth_core::{Json, Span};
use synth_weft::ast::{self, ItemKind, PortKind, StmtKind};

use crate::text::LineIndex;

// LSP `SymbolKind`s.
const MODULE: u32 = 2;
const PROPERTY: u32 = 7;
const FIELD: u32 = 8;
const ENUM: u32 = 10;
const FUNCTION: u32 = 12;
const VARIABLE: u32 = 13;
const CONSTANT: u32 = 14;
const ARRAY: u32 = 18;
const OBJECT: u32 = 19;
const ENUM_MEMBER: u32 = 22;
const STRUCT: u32 = 23;
const EVENT: u32 = 24;

fn sym(
    ix: &LineIndex,
    name: &ast::Ident,
    full: Span,
    kind: u32,
    detail: &str,
    children: Vec<Json>,
) -> Json {
    // The full range must contain the selection range.
    let start = full.start.min(name.span.start) as usize;
    let end = full.end.max(name.span.end) as usize;
    let mut s = Json::obj()
        .with("name", name.name.as_str())
        .with("kind", kind)
        .with("range", ix.range(start, end))
        .with("selectionRange", ix.range(name.span.start as usize, name.span.end as usize));
    if !detail.is_empty() {
        s.set("detail", detail);
    }
    if !children.is_empty() {
        s.set("children", children);
    }
    s
}

pub fn document_symbols(file: &ast::File, text: &str, ix: &LineIndex) -> Json {
    let snip = |sp: Span| -> String {
        let s = text.get(sp.start as usize..sp.end as usize).unwrap_or("");
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let mut out = Vec::new();
    for it in &file.items {
        match &it.kind {
            ItemKind::Module(m) => {
                let mut kids = Vec::new();
                for p in &m.ports {
                    let detail = match &p.kind {
                        PortKind::In { ty, .. } => format!("in {}", snip(ty.span)),
                        PortKind::Out { ty, .. } => format!("out {}", snip(ty.span)),
                        _ => snip(p.span),
                    };
                    kids.push(sym(ix, &p.name, p.span, PROPERTY, &detail, vec![]));
                }
                for st in m.body.iter().flatten() {
                    if let Some(s) = stmt_symbol(ix, st, &snip) {
                        kids.push(s);
                    }
                }
                let detail = if m.body.is_none() { "extern module" } else { "module" };
                out.push(sym(ix, &m.name, it.span, MODULE, detail, kids));
            }
            ItemKind::Function(f) => {
                let detail = format!("-> {}", snip(f.ret.span));
                out.push(sym(ix, &f.name, it.span, FUNCTION, &detail, vec![]));
            }
            ItemKind::Struct(s) => {
                let kids = s
                    .fields
                    .iter()
                    .map(|(n, t)| sym(ix, n, n.span.to(t.span), FIELD, &snip(t.span), vec![]))
                    .collect();
                out.push(sym(ix, &s.name, it.span, STRUCT, "struct", kids));
            }
            ItemKind::Enum(e) => {
                let kids = e
                    .variants
                    .iter()
                    .map(|(n, _)| sym(ix, n, n.span, ENUM_MEMBER, "", vec![]))
                    .collect();
                out.push(sym(ix, &e.name, it.span, ENUM, "enum", kids));
            }
            ItemKind::Const(c) => out.push(sym(ix, &c.name, it.span, CONSTANT, "const", vec![])),
            ItemKind::Assert(_) => {}
        }
    }
    Json::Arr(out)
}

fn stmt_symbol(ix: &LineIndex, st: &ast::Stmt, snip: &dyn Fn(Span) -> String) -> Option<Json> {
    Some(match &st.kind {
        StmtKind::Const(c) => sym(ix, &c.name, st.span, CONSTANT, "const", vec![]),
        StmtKind::Let { name, ty, .. } => {
            let detail = ty.as_ref().map(|t| snip(t.span)).unwrap_or_default();
            sym(ix, name, st.span, VARIABLE, &format!("let {detail}"), vec![])
        }
        StmtKind::Wire { name, ty, .. } => {
            sym(ix, name, st.span, VARIABLE, &format!("wire {}", snip(ty.span)), vec![])
        }
        StmtKind::Reg { name, ty, .. } => {
            sym(ix, name, st.span, FIELD, &format!("reg {}", snip(ty.span)), vec![])
        }
        StmtKind::Mem { name, elem, .. } => {
            sym(ix, name, st.span, ARRAY, &format!("mem {}", snip(elem.span)), vec![])
        }
        StmtKind::Inst { name, call } => {
            let callee = match &call.kind {
                ast::ExprKind::Call { callee, .. } => callee.name.clone(),
                _ => String::new(),
            };
            sym(ix, name, st.span, OBJECT, &format!("inst {callee}"), vec![])
        }
        StmtKind::Domain { name, .. } => sym(ix, name, st.span, EVENT, "domain", vec![]),
        _ => return None,
    })
}
