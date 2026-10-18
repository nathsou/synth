//! Verilog-2005 frontend: preprocessing, lexing and parsing into a syntax
//! tree ([`ast`]). Elaboration into RTL lives in `synth-elab`.

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod preproc;

use synth_core::{Diagnostics, FileId, SourceMap};

pub use preproc::PreprocOptions;

/// Preprocesses and parses `files` (sharing macros across them, in order).
pub fn parse_files(
    sm: &mut SourceMap,
    files: &[FileId],
    opts: &PreprocOptions,
) -> (ast::SourceText, Diagnostics) {
    let pp = preproc::preprocess(sm, files, opts);
    let mut diags = Diagnostics::default();
    diags.items.extend(pp.diagnostics);
    let parsed = parser::parse(&pp.tokens);
    diags.items.extend(parsed.diagnostics);
    (parsed.source, diags)
}
