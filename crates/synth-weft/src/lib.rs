//! Weft: a hardware description language compiled straight to the RTL IR.
//!
//! The language is specified in `docs/WEFT.md`. [`compile`] parses Weft
//! sources together with the standard library and any architecture
//! preludes, elaborates the hierarchy from the top module and returns one
//! flat [`synth_rtl::Module`].

pub mod ast;
mod check;
mod elab;
pub mod fmt;
pub mod lexer;
pub mod parser;
mod types;

pub use elab::{Options, RefKind, Reference};

use synth_core::{Diagnostics, FileId, SourceMap};

/// Names that cannot be redefined.
pub const BUILTINS: &[&str] = &[
    "concat",
    "from_bits",
    "unsafe_cross",
    "hex_file",
    "bin_file",
    "clog2",
    "bits_for",
    "min",
    "max",
    "assert",
];

/// The standard library source.
pub const STD: &str = include_str!("std.weft");

/// Lexes and parses one file.
pub fn parse_file(sm: &SourceMap, file: FileId) -> (ast::File, Diagnostics) {
    let mut diags = Diagnostics::default();
    let toks = lexer::lex(file, &sm.file(file).text, &mut diags);
    let f = parser::parse(&toks, &mut diags);
    (f, diags)
}

/// Compiles Weft files into one flat module.
///
/// `preludes` are `(name, source)` pairs of extern declarations for device
/// primitives, which become primitive instances. `verilog` provides the
/// definitions of other extern modules.
pub fn compile(
    sm: &mut SourceMap,
    files: &[FileId],
    preludes: &[(&str, &str)],
    verilog: Option<&synth_verilog::ast::SourceText>,
    opts: &Options,
) -> (Option<synth_rtl::Module>, Diagnostics) {
    let (m, diags, _, _) = run(sm, files, preludes, verilog, opts);
    (m, diags)
}

/// What an editor needs to know about a set of files.
#[derive(Debug, Default)]
pub struct Analysis {
    pub diags: Diagnostics,
    /// Every resolved name, in no particular order.
    pub refs: Vec<Reference>,
    /// False when syntax errors stopped the analysis before elaboration.
    pub parsed: bool,
}

/// Checks every module of `files` (whether or not a top can be found) and
/// indexes the names they use.
pub fn analyze(
    sm: &mut SourceMap,
    files: &[FileId],
    preludes: &[(&str, &str)],
    verilog: Option<&synth_verilog::ast::SourceText>,
    opts: &Options,
) -> Analysis {
    let opts = Options { check_all: true, ..opts.clone() };
    let (_, diags, refs, parsed) = run(sm, files, preludes, verilog, &opts);
    Analysis { diags, refs, parsed }
}

fn run(
    sm: &mut SourceMap,
    files: &[FileId],
    preludes: &[(&str, &str)],
    verilog: Option<&synth_verilog::ast::SourceText>,
    opts: &Options,
) -> (Option<synth_rtl::Module>, Diagnostics, Vec<Reference>, bool) {
    let std_id = sm.add("<weft std>", STD);
    let prelude_ids: Vec<FileId> =
        preludes.iter().map(|(n, t)| sm.add(format!("<{n}>"), *t)).collect();
    let mut all = vec![std_id];
    all.extend(&prelude_ids);
    all.extend(files);
    let mut diags = Diagnostics::default();
    let mut parsed = Vec::new();
    for &f in &all {
        let (ast, d) = parse_file(sm, f);
        diags.extend(d);
        parsed.push((f, ast));
    }
    if diags.has_errors() {
        return (None, diags, Vec::new(), false);
    }
    let refs: Vec<(FileId, &ast::File)> = parsed.iter().map(|(f, a)| (*f, a)).collect();
    let mut quiet = vec![std_id];
    quiet.extend(&prelude_ids);
    let (m, d, index) = elab::elaborate(sm, &refs, &quiet, &prelude_ids, verilog, opts);
    diags.extend(d);
    (m, diags, index, true)
}
