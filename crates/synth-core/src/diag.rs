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
///
/// The primary span may carry a short `label` printed under it. `labels`
/// mark secondary locations (shown with `-` instead of `^`), and `helps`
/// suggest fixes.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Option<Span>,
    pub label: Option<String>,
    pub labels: Vec<(Span, String)>,
    pub notes: Vec<(Option<Span>, String)>,
    pub helps: Vec<String>,
}

impl Diagnostic {
    pub fn new(severity: Severity, span: Option<Span>, message: impl Into<String>) -> Self {
        Diagnostic {
            severity,
            message: message.into(),
            span,
            label: None,
            labels: vec![],
            notes: vec![],
            helps: vec![],
        }
    }

    pub fn error(span: Span, message: impl Into<String>) -> Self {
        Diagnostic::new(Severity::Error, Some(span), message)
    }

    pub fn warning(span: Span, message: impl Into<String>) -> Self {
        Diagnostic::new(Severity::Warning, Some(span), message)
    }

    pub fn global_error(message: impl Into<String>) -> Self {
        Diagnostic::new(Severity::Error, None, message)
    }

    pub fn with_note(mut self, span: Option<Span>, note: impl Into<String>) -> Self {
        self.notes.push((span, note.into()));
        self
    }

    /// A short text shown under the primary span.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// A secondary location, shown with its own excerpt.
    pub fn with_secondary(mut self, span: Span, label: impl Into<String>) -> Self {
        self.labels.push((span, label.into()));
        self
    }

    /// A suggested fix, printed as `= help: ...`.
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.helps.push(help.into());
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
        let mut labels: Vec<(Span, char, Option<&str>)> = Vec::new();
        if let Some(span) = self.span {
            labels.push((span, '^', self.label.as_deref()));
        }
        for (span, label) in &self.labels {
            labels.push((*span, '-', Some(label)));
        }
        excerpts(&mut out, sm, &labels);
        for help in &self.helps {
            let _ = writeln!(out, "  = help: {help}");
        }
        for (span, note) in &self.notes {
            let _ = writeln!(out, "note: {note}");
            if let Some(span) = span {
                excerpts(&mut out, sm, &[(*span, '^', None)]);
            }
        }
        out
    }
}

/// Prints source excerpts for labelled spans. Labels on the same line share
/// one copy of the line; the first label's location heads the excerpt.
fn excerpts(out: &mut String, sm: &SourceMap, labels: &[(Span, char, Option<&str>)]) {
    /// A marker under a line: column, length, mark character and label.
    type Marker<'l> = (usize, usize, char, Option<&'l str>);
    // (file, line) -> markers, in order of first use.
    let mut groups: Vec<(FileId, usize, Vec<Marker<'_>>)> = Vec::new();
    for &(span, mark, label) in labels {
        if span.file.0 as usize >= sm.len() {
            continue;
        }
        let f = sm.file(span.file);
        let (line, col) = f.line_col(span.start as usize);
        let (end_line, end_col) = f.line_col(span.end as usize);
        let text = f.line_text(line);
        let len = if end_line == line {
            (end_col - col).max(1)
        } else {
            (text.chars().count() + 1 - col).max(1)
        };
        // A secondary label identical to the primary span adds nothing.
        let dup = groups.iter().any(|(gf, gl, ms)| {
            *gf == span.file
                && *gl == line
                && ms.iter().any(|m| m.0 == col && m.1 == len && m.3 == label)
        });
        if dup {
            continue;
        }
        match groups.iter_mut().find(|(gf, gl, _)| *gf == span.file && *gl == line) {
            Some(g) => g.2.push((col, len, mark, label)),
            None => groups.push((span.file, line, vec![(col, len, mark, label)])),
        }
    }
    if groups.is_empty() {
        return;
    }
    let width = groups.iter().map(|g| g.1.to_string().len()).max().unwrap_or(1);
    let mut cur_file = None;
    for (k, (file, line, marks)) in groups.iter().enumerate() {
        let f = sm.file(*file);
        let col = marks[0].0;
        if cur_file != Some(*file) {
            let arrow = if k == 0 { "-->" } else { ":::" };
            let _ = writeln!(out, "{:width$}{arrow} {}:{line}:{col}", "", f.name);
            let _ = writeln!(out, "{:width$} |", "");
            cur_file = Some(*file);
        } else if k > 0 && groups[k - 1].1 + 1 != *line {
            let _ = writeln!(out, "{:width$} |", "");
        }
        let _ = writeln!(out, "{line:>width$} | {}", f.line_text(*line));
        for (col, len, mark, label) in marks {
            let marks = mark.to_string().repeat(*len);
            match label {
                Some(l) => {
                    let _ = writeln!(out, "{:width$} | {:pad$}{marks} {l}", "", "", pad = col - 1);
                }
                None => {
                    let _ = writeln!(out, "{:width$} | {:pad$}{marks}", "", "", pad = col - 1);
                }
            }
        }
    }
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
