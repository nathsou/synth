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
//!
//! The one exception is a module header longer than [`MAX_WIDTH`]: its ports
//! go one per line, and so do its generic parameters if the header is still
//! too long without the ports:
//!
//! ```text
//! module top<
//!     CLK_HZ: int = 12_000_000,
//!     BAUD: int = 115200,
//! >(
//!     in  clk: clock,
//!     out tx: bool,
//! ) {
//! ```

/// The width past which a module header is broken over several lines.
pub const MAX_WIDTH: usize = 100;

/// Formats a Weft source. Formatting is idempotent.
pub fn format(src: &str) -> String {
    let mut lines: Vec<String> = src.lines().map(String::from).collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut depth: usize = 0;
    // Bracket kinds still open (`<` for a module's generic list), and
    // whether each `(` is a port list.
    let mut stack: Vec<(char, bool)> = Vec::new();
    // The lines that open a port list, to align.
    let mut port_lists = Vec::new();
    let mut in_block_comment = false;
    let mut continuation = false;
    let mut blank_run = 0;
    let mut k = 0;
    while k < lines.len() {
        let raw = lines[k].clone();
        k += 1;
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
        if depth == 0
            && text.chars().count() > MAX_WIDTH
            && let Some(split) = split_header(text)
        {
            // Lay out the header's lines in its place.
            k -= 1;
            lines.splice(k..k + 1, split);
            continue;
        }
        let closes_generics = text.starts_with('>') && stack.last() == Some(&('<', false));
        let closes_first = text.starts_with(['}', ')', ']']) || closes_generics;
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
        if s.code.strip_suffix('<').is_some_and(is_module_head) {
            stack.push(('<', false));
            depth += 1;
            continuation = false;
        }
        if s.code.ends_with('(') && stack.last() == Some(&('(', true)) {
            port_lists.push(out.len() - 1);
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    align_ports(&mut out, &port_lists);
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
fn scan(
    text: &str,
    stack: &mut Vec<(char, bool)>,
    depth: &mut usize,
    mut in_comment: bool,
) -> Scan {
    let b = text.as_bytes();
    let mut code = String::new();
    let mut i = 0;
    let mut in_str = false;
    let mut closed_generics = false;
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
            // The end of a module's generic list broken over lines.
            '>' if i == 0 && stack.last() == Some(&('<', false)) => {
                stack.pop();
                *depth = stack.len();
                closed_generics = true;
            }
            '(' => {
                let ports = is_port_list(&code) || closed_generics && code == ">";
                stack.push(('(', ports));
                *depth += 1;
            }
            '}' | ')' | ']' => {
                stack.pop();
                *depth = stack.len();
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
    is_module_head(t)
}

/// Whether code is `module name` (or `extern module name`).
fn is_module_head(code: &str) -> bool {
    let mut words = code.rsplit(|c: char| c.is_whitespace()).filter(|w| !w.is_empty());
    let name = words.next().unwrap_or("");
    !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && words.next() == Some("module")
}

/// A one-line module header too long for [`MAX_WIDTH`], broken over lines
/// (unindented): ports one per line, and generic parameters too if
/// `module name<...>(` alone is too long. `None` if the line is not a whole
/// header (or has a comment).
fn split_header(text: &str) -> Option<Vec<String>> {
    if text.contains("//") || text.contains("/*") {
        return None;
    }
    let after_kw = text.strip_prefix("extern ").unwrap_or(text).strip_prefix("module ")?;
    let name_end =
        after_kw.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(after_kw.len());
    let head_len = text.len() - after_kw.len() + name_end;
    let head = &text[..head_len];
    let mut rest = &text[head_len..];
    let mut generics = None;
    if rest.starts_with('<') {
        let end = close_of(rest)?;
        generics = Some(split_list(&rest[1..end]));
        rest = &rest[end + 1..];
    }
    if !rest.starts_with('(') {
        return None;
    }
    let end = close_of(rest)?;
    let ports = split_list(&rest[1..end]);
    let tail = rest[end + 1..].trim();
    if !matches!(tail, "{" | ";") {
        return None;
    }
    let mut out = Vec::new();
    let generics_text = match &generics {
        Some(g) => format!("<{}>", g.join(", ")),
        None => String::new(),
    };
    let mut open = format!("{head}{generics_text}(");
    if let Some(g) = generics.filter(|_| open.chars().count() > MAX_WIDTH) {
        out.push(format!("{head}<"));
        out.extend(g.into_iter().map(|p| format!("{p},")));
        open = ">(".into();
    }
    let close = if tail == "{" { ") {" } else { ");" };
    if ports.is_empty() && out.is_empty() {
        // Nothing to break.
        return None;
    }
    if ports.is_empty() {
        out.push(format!("{open}{close}"));
    } else {
        out.push(open);
        out.extend(ports.into_iter().map(|p| format!("{p},")));
        out.push(close.into());
    }
    Some(out)
}

/// The nesting depth after each byte of `s`, counting brackets and strings:
/// `<` counts only as a type or generic argument list (`uint<8>`, after a
/// name), and `>` only when it closes one (not in `->`, `=>` or `>=`).
fn depths(s: &str) -> Vec<usize> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut stack = Vec::new();
    let mut in_str = false;
    let mut escaped = false;
    for (i, &c) in b.iter().enumerate() {
        let prev = if i > 0 { b[i - 1] } else { b' ' };
        let next = b.get(i + 1).copied().unwrap_or(b' ');
        if in_str {
            out.push(stack.len() + 1);
            in_str = escaped || c != b'"';
            escaped = !escaped && c == b'\\';
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'(' | b'[' | b'{' => stack.push(c),
            b'<' if (i == 0 || prev.is_ascii_alphanumeric() || prev == b'_') && next != b'=' => {
                stack.push(c)
            }
            b'>' if stack.last() == Some(&b'<') && !matches!(prev, b'-' | b'=') && next != b'=' => {
                stack.pop();
            }
            b')' | b']' | b'}' => {
                // Drop a `<` that was a comparison after all.
                while stack.last() == Some(&b'<') {
                    stack.pop();
                }
                stack.pop();
            }
            _ => {}
        }
        out.push(stack.len() + usize::from(in_str));
    }
    out
}

/// The position of the bracket closing the one `s` starts with.
fn close_of(s: &str) -> Option<usize> {
    depths(s).iter().position(|&d| d == 0)
}

/// The items of a bracket's contents, split at its top-level commas and
/// trimmed (without an empty last item for a trailing comma).
fn split_list(s: &str) -> Vec<String> {
    let d = depths(s);
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == ',' && d[i] == 0 {
            out.push(s[start..i].trim().to_string());
            start = i + 1;
        }
    }
    out.push(s[start..].trim().to_string());
    out.retain(|x| !x.is_empty());
    out
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

/// Pads the directions of consecutive one-per-line ports to the same width,
/// in the port lists opened by the lines `opens`.
fn align_ports(lines: &mut [String], opens: &[usize]) {
    for &i in opens {
        // The port lines up to the closing parenthesis.
        let start = i + 1;
        let mut end = start;
        while end < lines.len() && !lines[end].trim_start().starts_with(')') {
            end += 1;
        }
        let ports: Vec<usize> = (start..end).filter(|&k| direction(&lines[k]).is_some()).collect();
        let width = ports
            .iter()
            .filter_map(|&k| direction(&lines[k]))
            .map(|(_, d)| d.len())
            .max()
            .unwrap_or(0);
        for k in ports {
            let (indent, d) = direction(&lines[k]).unwrap();
            let after = lines[k][indent + d.len()..].trim_start().to_string();
            lines[k] = format!("{}{d}{} {after}", &lines[k][..indent], " ".repeat(width - d.len()));
        }
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
    fn breaks_long_module_headers() {
        let src = "module top<CLK_HZ: int = 12_000_000, MAX_HZ: int = 25_000_000, SYS_HZ: int = min(CLK_HZ, MAX_HZ), CLKS_PER_BIT: int = SYS_HZ / 115200, DELAY: int = 1000>(in clk: clock, out tx: bool, out led: uint<8>) {\ntx = true;\n}\n";
        let want = "module top<\n    CLK_HZ: int = 12_000_000,\n    MAX_HZ: int = 25_000_000,\n    SYS_HZ: int = min(CLK_HZ, MAX_HZ),\n    CLKS_PER_BIT: int = SYS_HZ / 115200,\n    DELAY: int = 1000,\n>(\n    in  clk: clock,\n    out tx: bool,\n    out led: uint<8>,\n) {\n    tx = true;\n}\n";
        assert_eq!(format(src), want);
        assert_eq!(format(want), want);
        // Short generics stay on the module's line; extern modules end with `;`.
        // No ports, and generics that fit once the header's end goes.
        let long =
            format!("module m<{}>() {{\n}}\n", "A: int = 1, ".repeat(6) + "BBBBBB: int = 222");
        assert_eq!(long.lines().next().unwrap().len(), MAX_WIDTH + 3);
        assert_eq!(format(&long), long);
        let src = "extern module SB_PRIM<NAME: string = \"a, b\", W: int = 0>(in clock_input_pin: clock, out data_output_pin: uint<W> @async);\n";
        let want = "extern module SB_PRIM<NAME: string = \"a, b\", W: int = 0>(\n    in  clock_input_pin: clock,\n    out data_output_pin: uint<W> @async,\n);\n";
        assert_eq!(format(src), want);
        // A header broken by hand is indented the same way.
        let src = "module m<\nA: int = 1,\n        B: int = 2\n>(\n        in a: bool,\n  out y: bool,\n    ) {\ny = a;\n}\n";
        let want = "module m<\n    A: int = 1,\n    B: int = 2\n>(\n    in  a: bool,\n    out y: bool,\n) {\n    y = a;\n}\n";
        assert_eq!(format(src), want);
    }

    #[test]
    fn leaves_strings_comments_and_one_line_ports() {
        let src = "// a { comment\nmodule top(in clk: clock, out y: bool) {\n    /* {\n       ( */\n    let s = \"{(\";\n    y = match s {\n        _ => true,\n    };\n}\n";
        assert_eq!(format(src), src);
    }
}
