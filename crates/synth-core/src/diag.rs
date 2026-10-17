//! Source files, spans and diagnostics.
//!
//! Every frontend records source positions as [`Span`]s into a shared
//! [`SourceMap`] and reports problems as [`Diagnostic`]s, which render in a
//! compiler-style format with the offending source line.

use std::fmt::{self, Write as _};

/// Index of a file in a [`SourceMap`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct FileId(pub u32);

/// A byte range in a source file.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: FileId, start: usize, end: usize) -> Span {
        Span { file, start: start as u32, end: end as u32 }
    }

    /// The smallest span covering both (must be in the same file).
    pub fn to(self, other: Span) -> Span {
        if self.file != other.file {
            return self;
        }
        Span { file: self.file, start: self.start.min(other.start), end: self.end.max(other.end) }
    }
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub name: String,
    pub text: String,
    line_starts: Vec<usize>,
}

impl SourceFile {
    /// 1-based line and column of a byte offset.
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        let line = self.line_starts.partition_point(|&s| s <= offset) - 1;
        let col = self.text[self.line_starts[line]..offset.min(self.text.len())].chars().count();
        (line + 1, col + 1)
    }

    /// Text of 1-based line `line`, without the newline.
    pub fn line_text(&self, line: usize) -> &str {
        let start = self.line_starts[line - 1];
        let end = self.line_starts.get(line).copied().unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }
}

/// All source files of a compilation.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: impl Into<String>, text: impl Into<String>) -> FileId {
        let text = text.into();
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        self.files.push(SourceFile { name: name.into(), text, line_starts });
        FileId(self.files.len() as u32 - 1)
    }

    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// `file:line:col` of a span's start.
    pub fn location(&self, span: Span) -> String {
        let f = self.file(span.file);
        let (l, c) = f.line_col(span.start as usize);
        format!("{}:{l}:{c}", f.name)
    }

    pub fn snippet(&self, span: Span) -> &str {
        let f = self.file(span.file);
        &f.text[span.start as usize..(span.end as usize).min(f.text.len())]
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Note,
    Warning,
    Error,
}

/// A message about the source, with a primary location and optional notes.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Option<Span>,
    pub notes: Vec<(Option<Span>, String)>,
}

impl Diagnostic {
    pub fn error(span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span: Some(span),
            notes: vec![],
        }
    }

    pub fn warning(span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            message: message.into(),
            span: Some(span),
            notes: vec![],
        }
    }

    pub fn global_error(message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Error, message: message.into(), span: None, notes: vec![] }
    }

    pub fn with_note(mut self, span: Option<Span>, note: impl Into<String>) -> Self {
        self.notes.push((span, note.into()));
        self
    }

    /// Renders the diagnostic with source excerpts.
    pub fn render(&self, sm: &SourceMap) -> String {
        let mut out = String::new();
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        };
        let _ = writeln!(out, "{sev}: {}", self.message);
        if let Some(span) = self.span {
            excerpt(&mut out, sm, span);
        }
        for (span, note) in &self.notes {
            let _ = writeln!(out, "note: {note}");
            if let Some(span) = span {
                excerpt(&mut out, sm, *span);
            }
        }
        out
    }
}

fn excerpt(out: &mut String, sm: &SourceMap, span: Span) {
    if span.file.0 as usize >= sm.len() {
        return;
    }
    let f = sm.file(span.file);
    let (line, col) = f.line_col(span.start as usize);
    let (end_line, end_col) = f.line_col(span.end as usize);
    let text = f.line_text(line);
    let width = line.to_string().len();
    let _ = writeln!(out, "{:width$}--> {}:{line}:{col}", "", f.name);
    let _ = writeln!(out, "{:width$} |", "");
    let _ = writeln!(out, "{line} | {text}");
    let len = if end_line == line {
        (end_col - col).max(1)
    } else {
        (text.chars().count() + 1 - col).max(1)
    };
    let _ = writeln!(out, "{:width$} | {:pad$}{}", "", "", "^".repeat(len), pad = col - 1);
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Collects diagnostics while processing continues past recoverable errors.
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    pub items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn push(&mut self, d: Diagnostic) {
        self.items.push(d);
    }

    pub fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.push(Diagnostic::error(span, msg));
    }

    pub fn warning(&mut self, span: Span, msg: impl Into<String>) {
        self.push(Diagnostic::warning(span, msg));
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity == Severity::Error)
    }

    pub fn error_count(&self) -> usize {
        self.items.iter().filter(|d| d.severity == Severity::Error).count()
    }

    pub fn render(&self, sm: &SourceMap) -> String {
        self.items.iter().map(|d| d.render(sm)).collect::<Vec<_>>().join("\n")
    }

    pub fn extend(&mut self, other: Diagnostics) {
        self.items.extend(other.items);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_excerpt() {
        let mut sm = SourceMap::new();
        let f = sm.add("t.v", "module m;\n  wire a = b;\nendmodule\n");
        let span = Span::new(f, 21, 22);
        assert_eq!(sm.location(span), "t.v:2:12");
        let d = Diagnostic::error(span, "undeclared identifier `b`");
        let r = d.render(&sm);
        assert!(r.contains("error: undeclared identifier `b`"));
        assert!(r.contains("2 |   wire a = b;"));
        assert!(r.contains("  |            ^"), "{r}");
    }
}
