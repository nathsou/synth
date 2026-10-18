//! The Weft formatter.
//!
//! It is conservative: it changes layout, never the code on a line. Lines are
//! indented by four spaces per open bracket, with one more level for the
//! continuation of an unfinished statement; trailing whitespace and runs of
//! blank lines go; and in a module's port list written one port per line, the
//! directions are padded so that the port names line up:
//!
//! ```text
//! module uart_tx(
//!     in  start: bool,
//!     out tx: bool,
//! ) {
//! ```

/// Formats a Weft source. Formatting is idempotent.
pub fn format(src: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut depth: usize = 0;
    // Bracket kinds still open, and whether each `(` is a port list.
    let mut stack: Vec<(char, bool)> = Vec::new();
    let mut in_block_comment = false;
    let mut continuation = false;
    let mut blank_run = 0;
    for raw in &lines {
        let text = raw.trim();
        if text.is_empty() {
            blank_run += 1;
            if blank_run == 1 && !out.is_empty() {
                out.push(String::new());
            }
            continue;
        }
        blank_run = 0;
        if in_block_comment {
            // Inside a block comment: keep the line as written.
            out.push(raw.trim_end().to_string());
            in_block_comment = !scan(text, &mut stack, &mut depth, true).ends_open_comment;
            continue;
        }
        let closes_first = text.starts_with(['}', ')', ']']);
        let mut level = depth - usize::from(closes_first && depth > 0);
        if continuation && !closes_first && !text.starts_with("//") {
            level += 1;
        }
        let in_ports = stack.last().is_some_and(|&(c, ports)| c == '(' && ports);
        let line = format!("{}{}", "    ".repeat(level), text);
        out.push(line);
        let s = scan(text, &mut stack, &mut depth, false);
        in_block_comment = s.ends_open_comment;
        continuation = !s.code.is_empty()
            && !s.code.ends_with([';', '{', '}', ',', '(', '[', ')', ']'])
            && !s.code.ends_with("=>")
            && !s.code.starts_with('#')
            && !in_ports;
        // A line that only closes brackets ends the statement.
        if s.code.starts_with(['}', ')', ']']) && s.code.ends_with([')', ']', '}', ';', ',']) {
            continuation = false;
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    align_ports(&mut out);
    let mut s = out.join("\n");
    s.push('\n');
    s
}

struct Scan {
    /// The line without its comment, trimmed.
    code: String,
    ends_open_comment: bool,
}

/// Updates the bracket stack with a line's code (outside strings and
/// comments).
fn scan(text: &str, stack: &mut Vec<(char, bool)>, depth: &mut usize, mut in_comment: bool) -> Scan {
    let b = text.as_bytes();
    let mut code = String::new();
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i] as char;
        if in_comment {
            if text[i..].starts_with("*/") {
                in_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_str {
            code.push(c);
            if c == '\\' && i + 1 < b.len() {
                code.push(b[i + 1] as char);
                i += 2;
                continue;
            }
            in_str = c != '"';
            i += 1;
            continue;
        }
        if text[i..].starts_with("//") {
            break;
        }
        if text[i..].starts_with("/*") {
            in_comment = true;
            i += 2;
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => {
                stack.push((c, false));
                *depth += 1;
            }
            '(' => {
                let ports = is_port_list(&code);
                stack.push(('(', ports));
                *depth += 1;
            }
            '}' | ')' | ']' => {
                if stack.pop().is_some() {
                    *depth = depth.saturating_sub(1);
                }
            }
            _ => {}
        }
        // Keep multi-byte characters whole.
        let ch = text[i..].chars().next().unwrap();
        code.push(if c.is_ascii() { c } else { ch });
        i += ch.len_utf8();
    }
    Scan { code: code.trim().to_string(), ends_open_comment: in_comment }
}

/// Whether a `(` after this code opens a module's port list.
fn is_port_list(before: &str) -> bool {
    let t = before.trim_end();
    // Strip a generic parameter list.
    let t = match t.strip_suffix('>') {
        Some(_) => {
            let mut level = 0;
            let mut cut = None;
            for (i, c) in t.char_indices().rev() {
                match c {
                    '>' => level += 1,
                    '<' => {
                        level -= 1;
                        if level == 0 {
                            cut = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            match cut {
                Some(i) => t[..i].trim_end(),
                None => return false,
            }
        }
        None => t,
    };
    let mut words = t.rsplit(|c: char| c.is_whitespace()).filter(|w| !w.is_empty());
    let _name = words.next();
    words.next() == Some("module")
}

const DIRECTIONS: [&str; 3] = ["inout", "out", "in"];

/// The direction keyword a port line starts with, after attributes.
fn direction(line: &str) -> Option<(usize, &'static str)> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    DIRECTIONS
        .iter()
        .find(|d| rest.strip_prefix(**d).is_some_and(|r| r.starts_with(' ')))
        .map(|d| (indent, *d))
}

/// Pads the directions of consecutive one-per-line ports to the same width.
fn align_ports(lines: &mut [String]) {
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].trim_end().ends_with('(') || !is_port_list(lines[i].trim_end().trim_end_matches('(')) {
            i += 1;
            continue;
        }
        // The port lines up to the closing parenthesis.
        let start = i + 1;
        let mut end = start;
        while end < lines.len() && !lines[end].trim_start().starts_with(')') {
            end += 1;
        }
        let ports: Vec<usize> = (start..end).filter(|&k| direction(&lines[k]).is_some()).collect();
        let width = ports.iter().filter_map(|&k| direction(&lines[k])).map(|(_, d)| d.len()).max().unwrap_or(0);
        for k in ports {
            let (indent, d) = direction(&lines[k]).unwrap();
            let after = lines[k][indent + d.len()..].trim_start().to_string();
            lines[k] = format!("{}{d}{} {after}", &lines[k][..indent], " ".repeat(width - d.len()));
        }
        i = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indents_and_aligns() {
        let src = "module m<W: int = 8>(\n  in start: bool,\n        out   tx: bool,\n inout pad: bool,\n) {\nreg x: uint<W> = 0;   \n\n\n\nif start {\nx := x +%\n1;\n}\n  tx = x[0]; // done\n}\n\n";
        let want = "module m<W: int = 8>(\n    in    start: bool,\n    out   tx: bool,\n    inout pad: bool,\n) {\n    reg x: uint<W> = 0;\n\n    if start {\n        x := x +%\n            1;\n    }\n    tx = x[0]; // done\n}\n";
        assert_eq!(format(src), want);
        assert_eq!(format(want), want);
    }

    #[test]
    fn leaves_strings_comments_and_one_line_ports() {
        let src = "// a { comment\nmodule top(in clk: clock, out y: bool) {\n    /* {\n       ( */\n    let s = \"{(\";\n    y = match s {\n        _ => true,\n    };\n}\n";
        assert_eq!(format(src), src);
    }
}
