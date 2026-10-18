//! The Verilog preprocessor: `` `define `` (with arguments), `` `undef ``,
//! `` `ifdef `` / `` `ifndef `` / `` `elsif `` / `` `else `` / `` `endif ``,
//! `` `include `` and the directives that have no effect on synthesis.
//!
//! It works on tokens and produces the token stream the parser consumes,
//! without line ends or directives.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use synth_core::{Diagnostic, FileId, SourceMap, Span};

use crate::lexer::{Tok, Token, lex};

/// A macro definition.
#[derive(Clone, Debug)]
pub struct Macro {
    /// Formal parameters with optional default values; `None` for object-like macros.
    pub params: Option<Vec<(String, Option<Vec<Token>>)>>,
    pub body: Vec<Token>,
}

/// Preprocessor configuration.
#[derive(Clone, Debug, Default)]
pub struct PreprocOptions {
    pub include_dirs: Vec<PathBuf>,
    /// Predefined macros (`-DNAME=value`).
    pub defines: Vec<(String, String)>,
}

/// Result of preprocessing.
#[derive(Debug)]
pub struct Preprocessed {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
    /// Macros defined at the end of preprocessing (they persist across files).
    pub macros: HashMap<String, Macro>,
}

struct Source {
    tokens: Vec<Token>,
    pos: usize,
    /// Directory for resolving relative includes (files only).
    dir: Option<PathBuf>,
    /// Name of the macro being expanded, to stop recursion.
    macro_name: Option<String>,
}

#[derive(Clone, Copy)]
struct Cond {
    /// Whether the current branch is active.
    active: bool,
    /// Whether some branch of this conditional was already taken.
    taken: bool,
    /// Whether the enclosing region is active.
    parent: bool,
}

struct Pre<'a> {
    sm: &'a mut SourceMap,
    opts: &'a PreprocOptions,
    stack: Vec<Source>,
    macros: HashMap<String, Macro>,
    conds: Vec<Cond>,
    out: Vec<Token>,
    diags: Vec<Diagnostic>,
}

const IGNORED_LINE_DIRECTIVES: &[&str] = &[
    "timescale",
    "default_nettype",
    "resetall",
    "celldefine",
    "endcelldefine",
    "unconnected_drive",
    "nounconnected_drive",
    "line",
    "pragma",
    "begin_keywords",
    "end_keywords",
    "default_decay_time",
    "default_trireg_strength",
    "delay_mode_distributed",
    "delay_mode_path",
    "delay_mode_unit",
    "delay_mode_zero",
];

impl Pre<'_> {
    fn active(&self) -> bool {
        self.conds.last().is_none_or(|c| c.active)
    }

    fn next_raw(&mut self) -> Option<Token> {
        loop {
            let src = self.stack.last_mut()?;
            if src.pos < src.tokens.len() {
                let t = src.tokens[src.pos].clone();
                src.pos += 1;
                if t.tok == Tok::Eof {
                    continue;
                }
                return Some(t);
            }
            self.stack.pop();
        }
    }

    fn peek_raw(&self) -> Option<&Token> {
        for src in self.stack.iter().rev() {
            if let Some(t) = src.tokens[src.pos..].iter().find(|t| t.tok != Tok::Eof) {
                return Some(t);
            }
        }
        None
    }

    /// Tokens up to (not including) the end of the current line.
    fn rest_of_line(&mut self) -> Vec<Token> {
        let mut v = Vec::new();
        while let Some(t) = self.peek_raw() {
            if t.tok == Tok::Newline {
                break;
            }
            v.push(self.next_raw().unwrap());
        }
        v
    }

    fn ident_after(&mut self, directive: &Token) -> Option<String> {
        // Skip nothing but same-line tokens.
        match self.peek_raw() {
            Some(Token { tok: Tok::Ident(_), .. }) => {
                let Some(Token { tok: Tok::Ident(n), .. }) = self.next_raw() else {
                    unreachable!()
                };
                Some(n)
            }
            _ => {
                self.diags.push(Diagnostic::error(directive.span, "expected a macro name"));
                None
            }
        }
    }

    fn run(&mut self) {
        while let Some(t) = self.next_raw() {
            match &t.tok {
                Tok::Newline => {}
                Tok::Directive(d) => {
                    let d = d.clone();
                    self.directive(&d, &t);
                }
                _ => {
                    if self.active() {
                        self.out.push(t);
                    }
                }
            }
        }
        if !self.conds.is_empty() {
            let span = self.out.last().map(|t| t.span).unwrap_or_default();
            self.diags.push(Diagnostic::error(span, "missing `endif"));
        }
    }

    fn directive(&mut self, d: &str, t: &Token) {
        match d {
            "ifdef" | "ifndef" => {
                let name = self.ident_after(t).unwrap_or_default();
                let parent = self.active();
                let defined = self.macros.contains_key(&name);
                let cond = if d == "ifdef" { defined } else { !defined };
                self.conds.push(Cond { active: parent && cond, taken: cond, parent });
            }
            "elsif" => {
                let name = self.ident_after(t).unwrap_or_default();
                let defined = self.macros.contains_key(&name);
                match self.conds.last_mut() {
                    Some(c) => {
                        let take = !c.taken && defined;
                        c.active = c.parent && take;
                        c.taken |= take;
                    }
                    None => self.diags.push(Diagnostic::error(t.span, "`elsif without `ifdef")),
                }
            }
            "else" => match self.conds.last_mut() {
                Some(c) => {
                    c.active = c.parent && !c.taken;
                    c.taken = true;
                }
                None => self.diags.push(Diagnostic::error(t.span, "`else without `ifdef")),
            },
            "endif" => {
                if self.conds.pop().is_none() {
                    self.diags.push(Diagnostic::error(t.span, "`endif without `ifdef"));
                }
            }
            _ if !self.active() => {
                // Inside a false branch, `define and friends are skipped too.
                if d == "define" {
                    self.rest_of_line();
                }
            }
            "define" => self.define(t),
            "undef" => {
                if let Some(n) = self.ident_after(t) {
                    self.macros.remove(&n);
                }
            }
            "include" => self.include(t),
            _ if IGNORED_LINE_DIRECTIVES.contains(&d) => {
                self.rest_of_line();
            }
            "__FILE__" | "__LINE__" => {
                let (line, name) = {
                    let f = self.sm.file(t.span.file);
                    (f.line_col(t.span.start as usize).0, f.name.clone())
                };
                let tok =
                    if d == "__LINE__" { Tok::Decimal(line.to_string()) } else { Tok::Str(name) };
                self.out.push(Token { tok, span: t.span });
            }
            _ => self.expand(d, t),
        }
    }

    fn define(&mut self, t: &Token) {
        let Some(Token { tok: Tok::Ident(name), span: name_span }) = self.peek_raw().cloned()
        else {
            self.diags.push(Diagnostic::error(t.span, "expected a macro name after `define"));
            self.rest_of_line();
            return;
        };
        self.next_raw();
        let mut params = None;
        // A parameter list must follow the name without white space.
        if let Some(Token { tok: Tok::Op("("), span }) = self.peek_raw()
            && span.start == name_span.end
        {
            self.next_raw();
            let mut ps = Vec::new();
            loop {
                match self.next_raw() {
                    Some(Token { tok: Tok::Ident(p), .. }) => {
                        let mut default = None;
                        if matches!(self.peek_raw(), Some(Token { tok: Tok::Op("="), .. })) {
                            self.next_raw();
                            let mut dv = Vec::new();
                            while let Some(tk) = self.peek_raw() {
                                if matches!(tk.tok, Tok::Op(",") | Tok::Op(")") | Tok::Newline) {
                                    break;
                                }
                                dv.push(self.next_raw().unwrap());
                            }
                            default = Some(dv);
                        }
                        ps.push((p, default));
                    }
                    Some(Token { tok: Tok::Op(")"), .. }) if ps.is_empty() => break,
                    other => {
                        self.diags.push(Diagnostic::error(
                            other.map_or(t.span, |o| o.span),
                            "bad macro parameter list",
                        ));
                        self.rest_of_line();
                        return;
                    }
                }
                match self.next_raw() {
                    Some(Token { tok: Tok::Op(","), .. }) => {}
                    Some(Token { tok: Tok::Op(")"), .. }) => break,
                    other => {
                        self.diags.push(Diagnostic::error(
                            other.map_or(t.span, |o| o.span),
                            "expected `,` or `)` in macro parameters",
                        ));
                        self.rest_of_line();
                        return;
                    }
                }
            }
            params = Some(ps);
        }
        let body = self.rest_of_line();
        self.macros.insert(name, Macro { params, body });
    }

    fn include(&mut self, t: &Token) {
        let path = match self.next_raw() {
            Some(Token { tok: Tok::Str(p), .. }) => p,
            other => {
                self.diags.push(Diagnostic::error(
                    other.map_or(t.span, |o| o.span),
                    "expected a file name after `include",
                ));
                return;
            }
        };
        if self.stack.len() > 64 {
            self.diags.push(Diagnostic::error(t.span, "`include nested too deeply"));
            return;
        }
        let cur_dir = self.stack.iter().rev().find_map(|s| s.dir.clone());
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(d) = &cur_dir {
            candidates.push(d.join(&path));
        }
        candidates.extend(self.opts.include_dirs.iter().map(|d| d.join(&path)));
        candidates.push(PathBuf::from(&path));
        let Some((found, text)) =
            candidates.iter().find_map(|c| std::fs::read_to_string(c).ok().map(|t| (c.clone(), t)))
        else {
            self.diags
                .push(Diagnostic::error(t.span, format!("cannot find include file \"{path}\"")));
            return;
        };
        let id = self.sm.add(found.to_string_lossy(), text);
        let mut d = Vec::new();
        let tokens = lex(id, &self.sm.file(id).text.clone(), &mut d);
        self.diags.extend(d);
        self.stack.push(Source {
            tokens,
            pos: 0,
            dir: found.parent().map(Path::to_path_buf),
            macro_name: None,
        });
    }

    fn expand(&mut self, name: &str, t: &Token) {
        let Some(m) = self.macros.get(name).cloned() else {
            self.diags.push(Diagnostic::error(t.span, format!("undefined macro `{name}")));
            return;
        };
        if self.stack.iter().filter(|s| s.macro_name.as_deref() == Some(name)).count() > 0
            || self.stack.len() > 200
        {
            self.diags
                .push(Diagnostic::error(t.span, format!("recursive expansion of macro `{name}")));
            return;
        }
        let mut body = m.body.clone();
        if let Some(params) = &m.params {
            // Arguments: `( ... )`, split at top-level commas.
            while matches!(self.peek_raw(), Some(Token { tok: Tok::Newline, .. })) {
                self.next_raw();
            }
            if !matches!(self.peek_raw(), Some(Token { tok: Tok::Op("("), .. })) {
                self.diags
                    .push(Diagnostic::error(t.span, format!("macro `{name} needs arguments")));
                return;
            }
            self.next_raw();
            let mut args: Vec<Vec<Token>> = vec![Vec::new()];
            let mut depth = 0;
            loop {
                let Some(tk) = self.next_raw() else {
                    self.diags.push(Diagnostic::error(
                        t.span,
                        format!("unterminated arguments of macro `{name}"),
                    ));
                    return;
                };
                match tk.tok {
                    Tok::Op("(" | "[" | "{") => depth += 1,
                    Tok::Op(")") if depth == 0 => break,
                    Tok::Op(")" | "]" | "}") => depth -= 1,
                    Tok::Op(",") if depth == 0 => {
                        args.push(Vec::new());
                        continue;
                    }
                    Tok::Newline => continue,
                    _ => {}
                }
                args.last_mut().unwrap().push(tk);
            }
            if args.len() == 1 && args[0].is_empty() && params.is_empty() {
                args.clear();
            }
            if args.len() > params.len() {
                self.diags.push(Diagnostic::error(
                    t.span,
                    format!("too many arguments for macro `{name}"),
                ));
                return;
            }
            let mut actual: HashMap<&str, Vec<Token>> = HashMap::new();
            for (i, (p, default)) in params.iter().enumerate() {
                let a = match args.get(i) {
                    Some(a) if !a.is_empty() => a.clone(),
                    _ => match default {
                        Some(d) => d.clone(),
                        None if args.get(i).is_some() => Vec::new(),
                        None => {
                            self.diags.push(Diagnostic::error(
                                t.span,
                                format!("missing argument `{p}` of macro `{name}"),
                            ));
                            return;
                        }
                    },
                };
                actual.insert(p.as_str(), a);
            }
            body = body
                .into_iter()
                .flat_map(|bt| match &bt.tok {
                    Tok::Ident(n) if actual.contains_key(n.as_str()) => actual[n.as_str()].clone(),
                    _ => vec![bt],
                })
                .collect();
        }
        // Expanded tokens report the location of the macro use.
        for bt in &mut body {
            bt.span = t.span;
        }
        self.stack.push(Source {
            tokens: body,
            pos: 0,
            dir: None,
            macro_name: Some(name.to_string()),
        });
    }
}

/// Preprocesses the files in `files`, in order, sharing macro definitions
/// between them (as when they are passed together to a compiler).
pub fn preprocess(sm: &mut SourceMap, files: &[FileId], opts: &PreprocOptions) -> Preprocessed {
    let mut macros = HashMap::new();
    for (k, v) in &opts.defines {
        let mut d = Vec::new();
        let id = sm.add(format!("<define {k}>"), v.clone());
        let mut body = lex(id, v, &mut d);
        body.retain(|t| !matches!(t.tok, Tok::Eof | Tok::Newline));
        macros.insert(k.clone(), Macro { params: None, body });
    }
    let mut pre = Pre {
        sm,
        opts,
        stack: Vec::new(),
        macros,
        conds: Vec::new(),
        out: Vec::new(),
        diags: Vec::new(),
    };
    for &f in files {
        let text = pre.sm.file(f).text.clone();
        let name = pre.sm.file(f).name.clone();
        let mut d = Vec::new();
        let tokens = lex(f, &text, &mut d);
        pre.diags.extend(d);
        let dir = Path::new(&name).parent().map(Path::to_path_buf);
        pre.stack.push(Source { tokens, pos: 0, dir, macro_name: None });
        pre.run();
        pre.conds.clear();
    }
    let eof_span = pre.out.last().map(|t| Span { start: t.span.end, ..t.span }).unwrap_or_default();
    pre.out.push(Token { tok: Tok::Eof, span: eof_span });
    Preprocessed { tokens: pre.out, diagnostics: pre.diags, macros: pre.macros }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> (Vec<Tok>, Vec<Diagnostic>) {
        let mut sm = SourceMap::new();
        let f = sm.add("t.v", src);
        let p = preprocess(
            &mut sm,
            &[f],
            &PreprocOptions { defines: vec![("PRE".into(), "7".into())], ..Default::default() },
        );
        (p.tokens.into_iter().map(|t| t.tok).filter(|t| *t != Tok::Eof).collect(), p.diagnostics)
    }

    fn ids(toks: &[Tok]) -> Vec<String> {
        toks.iter()
            .map(|t| match t {
                Tok::Ident(s) => s.clone(),
                Tok::Decimal(s) => s.clone(),
                Tok::Op(o) => o.to_string(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn defines_and_conditionals() {
        let (t, d) = run(
            "`define W 8\n`define ADD(a, b) (a + b)\n`ifdef W\nx `W\n`else\nnope\n`endif\n\
             `ifndef W\nno\n`elsif PRE\ny `ADD(1, (2,3)) `PRE\n`endif\n`define MULTI 1 \\\n + 2\nz `MULTI\n",
        );
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(
            ids(&t),
            ["x", "8", "y", "(", "1", "+", "(", "2", ",", "3", ")", ")", "7", "z", "1", "+", "2"]
        );
    }

    #[test]
    fn nested_false_branches_and_errors() {
        let (t, d) =
            run("`ifdef NOPE\n`ifdef X\na\n`else\nb\n`endif\n`define Q 1\n`else\nc\n`endif\n`Q\n");
        assert_eq!(ids(&t), ["c"]);
        assert_eq!(d.len(), 1, "undefined macro Q expected");
        let (_, d) = run("`ifdef A\n");
        assert!(d.iter().any(|x| x.message.contains("endif")));
    }

    #[test]
    fn object_macro_with_paren_after_space() {
        let (t, d) = run("`define M (x)\na `M\n");
        assert!(d.is_empty());
        assert_eq!(ids(&t), ["a", "(", "x", ")"]);
    }
}
