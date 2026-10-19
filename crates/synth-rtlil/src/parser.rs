//! Parser for the RTLIL text format, as written by Yosys `write_rtlil` and
//! Amaranth. RTLIL is line-oriented: each statement occupies one line.

use std::collections::HashMap;
use std::fmt;

use synth_netlist::{Bit, Param};

use crate::ast::*;

/// A parse error with its 1-based line number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RTLIL line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Word(String),
    Str(String),
    Const(Vec<Bit>),
    Int(i64),
    LBrace,
    RBrace,
    /// `[hi:lo]` or `[i]` (then `hi == lo`).
    Slice(i64, i64),
}

fn tokenize(line: &str, ln: usize) -> Result<Vec<Tok>, ParseError> {
    let err = |m: String| ParseError { line: ln, message: m };
    let b = line.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        // Commas only separate `case` values; treat them as whitespace.
        if c.is_ascii_whitespace() || c == b',' {
            i += 1;
            continue;
        }
        if c == b'#' {
            break;
        }
        match c {
            b'{' => {
                out.push(Tok::LBrace);
                i += 1;
            }
            b'}' => {
                out.push(Tok::RBrace);
                i += 1;
            }
            b'[' => {
                let end = line[i..].find(']').ok_or_else(|| err("unterminated slice".into()))? + i;
                let inner: String =
                    line[i + 1..end].chars().filter(|c| !c.is_whitespace()).collect();
                let parse = |s: &str| s.parse::<i64>().map_err(|_| err(format!("bad index `{s}`")));
                let (hi, lo) = match inner.split_once(':') {
                    Some((h, l)) => (parse(h)?, parse(l)?),
                    None => {
                        let v = parse(&inner)?;
                        (v, v)
                    }
                };
                out.push(Tok::Slice(hi, lo));
                i = end + 1;
            }
            b'"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    let Some(&ch) = b.get(i) else { return Err(err("unterminated string".into())) };
                    match ch {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\\' => {
                            let next = *b.get(i + 1).ok_or_else(|| err("bad escape".into()))?;
                            match next {
                                b'n' => s.push('\n'),
                                b't' => s.push('\t'),
                                b'0'..=b'7' => {
                                    // Up to three octal digits.
                                    let mut v = 0u32;
                                    let mut k = 0;
                                    while k < 3 && matches!(b.get(i + 1 + k), Some(b'0'..=b'7')) {
                                        v = v * 8 + (b[i + 1 + k] - b'0') as u32;
                                        k += 1;
                                    }
                                    s.push(char::from_u32(v).unwrap_or('?'));
                                    i += k - 1;
                                }
                                other => s.push(other as char),
                            }
                            i += 2;
                        }
                        _ => {
                            // Copy one UTF-8 character.
                            let ch_len = line[i..].chars().next().map_or(1, char::len_utf8);
                            s.push_str(&line[i..i + ch_len]);
                            i += ch_len;
                        }
                    }
                }
                out.push(Tok::Str(s));
            }
            b'\\' | b'$' => {
                let start = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                let raw = &line[start..i];
                // `\name` is a public identifier; keep `$name` as is.
                out.push(Tok::Ident(raw.strip_prefix('\\').unwrap_or(raw).to_string()));
            }
            _ => {
                let start = i;
                while i < b.len()
                    && !b[i].is_ascii_whitespace()
                    && !matches!(b[i], b'{' | b'}' | b'[' | b',')
                {
                    i += 1;
                }
                let w = &line[start..i];
                if let Some((width, bits)) = w.split_once('\'') {
                    let width: usize =
                        width.parse().map_err(|_| err(format!("bad constant `{w}`")))?;
                    // `s` marks a signed constant: short values sign-extend.
                    let (signed, bits) = match bits.strip_prefix('s') {
                        Some(rest) => (true, rest),
                        None => (false, bits),
                    };
                    let mut v: Vec<Bit> = bits
                        .chars()
                        .rev()
                        .map(|c| match c {
                            'm' | '-' => Some(Bit::X),
                            _ => Bit::from_char(c),
                        })
                        .collect::<Option<_>>()
                        .ok_or_else(|| err(format!("bad constant `{w}`")))?;
                    let fill =
                        if signed { v.last().copied().unwrap_or(Bit::Zero) } else { Bit::Zero };
                    v.resize(width, fill);
                    out.push(Tok::Const(v));
                } else if w.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
                    out.push(Tok::Int(w.parse().map_err(|_| err(format!("bad number `{w}`")))?));
                } else {
                    out.push(Tok::Word(w.to_string()));
                }
            }
        }
    }
    Ok(out)
}

struct Line {
    no: usize,
    toks: Vec<Tok>,
}

struct Parser<'a> {
    lines: Vec<Line>,
    pos: usize,
    /// Wires declared so far in the current module, for slice resolution.
    wires: HashMap<String, (usize, i64, bool)>,
    _src: std::marker::PhantomData<&'a ()>,
}

/// Cursor over the tokens of one line.
struct Toks<'t> {
    toks: &'t [Tok],
    i: usize,
    line: usize,
}

impl<'t> Toks<'t> {
    fn err<T>(&self, m: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError { line: self.line, message: m.into() })
    }

    fn peek(&self) -> Option<&'t Tok> {
        self.toks.get(self.i)
    }

    fn next(&mut self) -> Option<&'t Tok> {
        let t = self.toks.get(self.i);
        self.i += 1;
        t
    }

    fn ident(&mut self) -> Result<String, ParseError> {
        match self.next() {
            Some(Tok::Ident(s)) => Ok(s.clone()),
            other => self.err(format!("expected identifier, found {other:?}")),
        }
    }

    fn int(&mut self) -> Result<i64, ParseError> {
        match self.next() {
            Some(Tok::Int(v)) => Ok(*v),
            other => self.err(format!("expected integer, found {other:?}")),
        }
    }

    fn word_is(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(x)) if x == w)
    }

    fn end(&self) -> Result<(), ParseError> {
        if self.i < self.toks.len() {
            return self.err(format!("unexpected {:?}", self.toks[self.i]));
        }
        Ok(())
    }

    fn param_value(&mut self) -> Result<Param, ParseError> {
        match self.next() {
            Some(Tok::Const(b)) => Ok(Param::Bits(b.clone())),
            Some(Tok::Int(v)) => Ok(Param::Int(*v)),
            Some(Tok::Str(s)) => Ok(Param::Str(s.clone())),
            other => self.err(format!("expected constant, found {other:?}")),
        }
    }
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Result<Self, ParseError> {
        let mut lines = Vec::new();
        for (i, l) in text.lines().enumerate() {
            let toks = tokenize(l, i + 1)?;
            if !toks.is_empty() {
                lines.push(Line { no: i + 1, toks });
            }
        }
        Ok(Parser { lines, pos: 0, wires: HashMap::new(), _src: std::marker::PhantomData })
    }

    fn eof_line(&self) -> usize {
        self.lines.last().map_or(0, |l| l.no)
    }

    /// Returns a cursor over the next line and advances.
    fn line(&mut self) -> Result<Toks<'_>, ParseError> {
        let Some(l) = self.lines.get(self.pos) else {
            return Err(ParseError {
                line: self.eof_line(),
                message: "unexpected end of file".into(),
            });
        };
        self.pos += 1;
        Ok(Toks { toks: &l.toks, i: 0, line: l.no })
    }

    /// First word of the next line, without consuming it.
    fn peek_keyword(&self) -> Option<&str> {
        match self.lines.get(self.pos)?.toks.first()? {
            Tok::Word(w) => Some(w),
            _ => None,
        }
    }

    fn sigspec(&self, t: &mut Toks<'_>) -> Result<SigSpec, ParseError> {
        let mut chunks = match t.next() {
            Some(Tok::LBrace) => {
                let mut parts: Vec<SigSpec> = Vec::new();
                while !matches!(t.peek(), Some(Tok::RBrace)) {
                    if t.peek().is_none() {
                        return t.err("unterminated `{`");
                    }
                    parts.push(self.sigspec(t)?);
                }
                t.next();
                // Concatenation lists the most significant part first.
                parts.into_iter().rev().flat_map(|p| p.chunks).collect()
            }
            Some(Tok::Const(b)) => vec![SigChunk::Const(b.clone())],
            Some(Tok::Int(v)) => {
                vec![SigChunk::Const(
                    (0..32).map(|i| if v >> i & 1 != 0 { Bit::One } else { Bit::Zero }).collect(),
                )]
            }
            Some(Tok::Ident(name)) => {
                let Some(&(width, _, _)) = self.wires.get(name) else {
                    return t.err(format!("undeclared wire `{name}`"));
                };
                vec![SigChunk::Wire { wire: name.clone(), offset: 0, width }]
            }
            other => return t.err(format!("expected signal, found {other:?}")),
        };
        // Optional slice applies to the signal just parsed.
        while let Some(Tok::Slice(hi, lo)) = t.peek() {
            t.next();
            let (hi, lo) = (*hi, *lo);
            let (start, width) = match chunks.as_slice() {
                [SigChunk::Wire { wire, .. }] => {
                    let (w_width, w_off, upto) = self.wires[wire];
                    let idx = |i: i64| -> Result<usize, ParseError> {
                        let rel = i - w_off;
                        if rel < 0 || rel as usize >= w_width {
                            return t.err(format!("index {i} out of range for `{wire}`"));
                        }
                        Ok(if upto { w_width - 1 - rel as usize } else { rel as usize })
                    };
                    let (a, b) = (idx(hi)?, idx(lo)?);
                    (a.min(b), a.abs_diff(b) + 1)
                }
                _ => {
                    // Slicing a concatenation or constant: work on bit positions.
                    let (a, b) = (hi.max(lo) as usize, hi.min(lo) as usize);
                    (b, a - b + 1)
                }
            };
            chunks = slice_chunks(&chunks, start, width)
                .ok_or_else(|| ParseError { line: t.line, message: "slice out of range".into() })?;
        }
        Ok(SigSpec { chunks })
    }

    fn attrs_into(
        &self,
        t: &mut Toks<'_>,
        attrs: &mut Vec<(String, Param)>,
    ) -> Result<(), ParseError> {
        let name = t.ident()?;
        let v = t.param_value()?;
        t.end()?;
        attrs.push((name, v));
        Ok(())
    }

    fn design(mut self) -> Result<Design, ParseError> {
        let mut design = Design::default();
        let mut attrs = Vec::new();
        while self.pos < self.lines.len() {
            let kw = self.peek_keyword().map(str::to_owned);
            match kw.as_deref() {
                Some("autoidx") => {
                    self.pos += 1;
                }
                Some("attribute") => {
                    let mut t_attrs = Vec::new();
                    {
                        let l = &self.lines[self.pos];
                        let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                        self.attrs_into(&mut t, &mut t_attrs)?;
                    }
                    self.pos += 1;
                    attrs.extend(t_attrs);
                }
                Some("module") => {
                    let m = self.module(std::mem::take(&mut attrs))?;
                    design.modules.push(m);
                }
                _ => {
                    let l = &self.lines[self.pos];
                    return Err(ParseError {
                        line: l.no,
                        message: format!("unexpected {:?}", l.toks[0]),
                    });
                }
            }
        }
        Ok(design)
    }

    fn module(&mut self, attrs: Vec<(String, Param)>) -> Result<Module, ParseError> {
        let name = {
            let mut t = self.line()?;
            t.next();
            let n = t.ident()?;
            t.end()?;
            n
        };
        self.wires.clear();
        let mut m = Module { name, attrs, ..Default::default() };
        let mut pending: Vec<(String, Param)> = Vec::new();
        loop {
            let kw = self.peek_keyword().map(str::to_owned);
            let Some(kw) = kw else {
                let l = self.line()?;
                return l.err("expected statement");
            };
            match kw.as_str() {
                "end" => {
                    let mut t = self.line()?;
                    t.next();
                    t.end()?;
                    return Ok(m);
                }
                "attribute" => {
                    let mut tmp = Vec::new();
                    {
                        let l = &self.lines[self.pos];
                        let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                        self.attrs_into(&mut t, &mut tmp)?;
                    }
                    self.pos += 1;
                    pending.extend(tmp);
                }
                "parameter" => {
                    let mut t = self.line()?;
                    t.next();
                    while t.word_is("signed") || t.word_is("real") {
                        t.next();
                    }
                    let name = t.ident()?;
                    let v = if t.peek().is_some() { Some(t.param_value()?) } else { None };
                    t.end()?;
                    m.params.push((name, v));
                }
                "wire" => {
                    let mut t = self.line()?;
                    t.next();
                    let mut w = Wire {
                        name: String::new(),
                        width: 1,
                        offset: 0,
                        upto: false,
                        signed: false,
                        port: None,
                        attrs: std::mem::take(&mut pending),
                    };
                    loop {
                        match t.next() {
                            Some(Tok::Word(o)) => match o.as_str() {
                                "width" => w.width = t.int()? as usize,
                                "offset" => w.offset = t.int()?,
                                "upto" => w.upto = true,
                                "signed" => w.signed = true,
                                "input" => w.port = Some((PortDir::Input, t.int()? as u32)),
                                "output" => w.port = Some((PortDir::Output, t.int()? as u32)),
                                "inout" => w.port = Some((PortDir::Inout, t.int()? as u32)),
                                other => return t.err(format!("unknown wire option `{other}`")),
                            },
                            Some(Tok::Ident(n)) => {
                                w.name = n.clone();
                                break;
                            }
                            other => return t.err(format!("bad wire declaration at {other:?}")),
                        }
                    }
                    t.end()?;
                    self.wires.insert(w.name.clone(), (w.width, w.offset, w.upto));
                    m.wires.push(w);
                }
                "memory" => {
                    let mut t = self.line()?;
                    t.next();
                    let mut mem = Memory {
                        name: String::new(),
                        width: 1,
                        size: 0,
                        offset: 0,
                        attrs: std::mem::take(&mut pending),
                    };
                    loop {
                        match t.next() {
                            Some(Tok::Word(o)) => match o.as_str() {
                                "width" => mem.width = t.int()? as usize,
                                "size" => mem.size = t.int()? as usize,
                                "offset" => mem.offset = t.int()?,
                                other => return t.err(format!("unknown memory option `{other}`")),
                            },
                            Some(Tok::Ident(n)) => {
                                mem.name = n.clone();
                                break;
                            }
                            other => return t.err(format!("bad memory declaration at {other:?}")),
                        }
                    }
                    t.end()?;
                    m.memories.push(mem);
                }
                "cell" => {
                    let c = self.cell(std::mem::take(&mut pending))?;
                    m.cells.push(c);
                }
                "connect" => {
                    let pair = {
                        let l = &self.lines[self.pos];
                        let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                        let a = self.sigspec(&mut t)?;
                        let b = self.sigspec(&mut t)?;
                        t.end()?;
                        (a, b)
                    };
                    self.pos += 1;
                    m.connections.push(pair);
                }
                "process" => {
                    let p = self.process(std::mem::take(&mut pending))?;
                    m.processes.push(p);
                }
                other => {
                    let l = self.line()?;
                    return l.err(format!("unexpected `{other}` in module"));
                }
            }
        }
    }

    fn cell(&mut self, attrs: Vec<(String, Param)>) -> Result<Cell, ParseError> {
        let (kind, name) = {
            let mut t = self.line()?;
            t.next();
            let k = t.ident()?;
            let n = t.ident()?;
            t.end()?;
            (k, n)
        };
        let mut c = Cell { kind, name, params: vec![], connections: vec![], attrs };
        loop {
            let kw = self.peek_keyword().map(str::to_owned);
            match kw.as_deref() {
                Some("end") => {
                    self.pos += 1;
                    return Ok(c);
                }
                Some("parameter") => {
                    let mut t = self.line()?;
                    t.next();
                    while t.word_is("signed") || t.word_is("real") {
                        t.next();
                    }
                    let n = t.ident()?;
                    let v = t.param_value()?;
                    t.end()?;
                    c.params.push((n, v));
                }
                Some("connect") => {
                    let conn = {
                        let l = &self.lines[self.pos];
                        let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                        let port = t.ident()?;
                        let sig = self.sigspec(&mut t)?;
                        t.end()?;
                        (port, sig)
                    };
                    self.pos += 1;
                    c.connections.push(conn);
                }
                _ => {
                    let l = self.line()?;
                    return l.err("expected `parameter`, `connect` or `end` in cell");
                }
            }
        }
    }

    fn case_body(&mut self, body: &mut CaseBody) -> Result<(), ParseError> {
        let mut pending = Vec::new();
        loop {
            let kw = self.peek_keyword().map(str::to_owned);
            match kw.as_deref() {
                Some("assign") => {
                    let pair = {
                        let l = &self.lines[self.pos];
                        let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                        let a = self.sigspec(&mut t)?;
                        let b = self.sigspec(&mut t)?;
                        t.end()?;
                        (a, b)
                    };
                    self.pos += 1;
                    body.actions.push(pair);
                }
                Some("attribute") => {
                    let mut tmp = Vec::new();
                    {
                        let l = &self.lines[self.pos];
                        let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                        self.attrs_into(&mut t, &mut tmp)?;
                    }
                    self.pos += 1;
                    pending.extend(tmp);
                }
                Some("switch") => {
                    let sw = self.switch(std::mem::take(&mut pending))?;
                    body.switches.push(sw);
                }
                _ => return Ok(()),
            }
        }
    }

    fn switch(&mut self, attrs: Vec<(String, Param)>) -> Result<Switch, ParseError> {
        let signal = {
            let l = &self.lines[self.pos];
            let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
            let s = self.sigspec(&mut t)?;
            t.end()?;
            s
        };
        self.pos += 1;
        let mut sw = Switch { signal, cases: vec![], attrs };
        let mut pending = Vec::new();
        loop {
            let kw = self.peek_keyword().map(str::to_owned);
            match kw.as_deref() {
                Some("attribute") => {
                    let mut tmp = Vec::new();
                    {
                        let l = &self.lines[self.pos];
                        let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                        self.attrs_into(&mut t, &mut tmp)?;
                    }
                    self.pos += 1;
                    pending.extend(tmp);
                }
                Some("case") => {
                    let compare = {
                        let l = &self.lines[self.pos];
                        let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                        let mut cmp = Vec::new();
                        while t.peek().is_some() {
                            cmp.push(self.sigspec(&mut t)?);
                        }
                        cmp
                    };
                    self.pos += 1;
                    let mut body = CaseBody::default();
                    self.case_body(&mut body)?;
                    sw.cases.push(Case { compare, body, attrs: std::mem::take(&mut pending) });
                }
                Some("end") => {
                    self.pos += 1;
                    return Ok(sw);
                }
                _ => {
                    let l = self.line()?;
                    return l.err("expected `case` or `end` in switch");
                }
            }
        }
    }

    fn process(&mut self, attrs: Vec<(String, Param)>) -> Result<Process, ParseError> {
        let name = {
            let mut t = self.line()?;
            t.next();
            let n = t.ident()?;
            t.end()?;
            n
        };
        let mut p = Process { name, root: CaseBody::default(), syncs: vec![], attrs };
        self.case_body(&mut p.root)?;
        loop {
            let kw = self.peek_keyword().map(str::to_owned);
            match kw.as_deref() {
                Some("sync") => {
                    let kind = {
                        let l = &self.lines[self.pos];
                        let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                        let k = match t.next() {
                            Some(Tok::Word(w)) => w.clone(),
                            other => return t.err(format!("bad sync rule {other:?}")),
                        };
                        let kind = match k.as_str() {
                            "low" => SyncKind::Low(self.sigspec(&mut t)?),
                            "high" => SyncKind::High(self.sigspec(&mut t)?),
                            "posedge" => SyncKind::Posedge(self.sigspec(&mut t)?),
                            "negedge" => SyncKind::Negedge(self.sigspec(&mut t)?),
                            "edge" => SyncKind::Edge(self.sigspec(&mut t)?),
                            "always" => SyncKind::Always,
                            "global" => SyncKind::Global,
                            "init" => SyncKind::Init,
                            other => return t.err(format!("unknown sync type `{other}`")),
                        };
                        t.end()?;
                        kind
                    };
                    self.pos += 1;
                    let mut sync = Sync { kind, updates: vec![], memwrites: vec![] };
                    loop {
                        let kw = self.peek_keyword().map(str::to_owned);
                        match kw.as_deref() {
                            Some("update") => {
                                let pair = {
                                    let l = &self.lines[self.pos];
                                    let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                                    let a = self.sigspec(&mut t)?;
                                    let b = self.sigspec(&mut t)?;
                                    t.end()?;
                                    (a, b)
                                };
                                self.pos += 1;
                                sync.updates.push(pair);
                            }
                            Some("memwr") => {
                                let mw = {
                                    let l = &self.lines[self.pos];
                                    let mut t = Toks { toks: &l.toks[1..], i: 0, line: l.no };
                                    let memory = t.ident()?;
                                    let address = self.sigspec(&mut t)?;
                                    let data = self.sigspec(&mut t)?;
                                    let enable = self.sigspec(&mut t)?;
                                    let priority = match t.peek() {
                                        Some(Tok::Int(_)) => t.int()?,
                                        Some(Tok::Const(b)) => {
                                            t.next();
                                            Param::Bits(b.clone()).as_u64().unwrap_or(0) as i64
                                        }
                                        _ => 0,
                                    };
                                    t.end()?;
                                    MemWrite { memory, address, data, enable, priority }
                                };
                                self.pos += 1;
                                sync.memwrites.push(mw);
                            }
                            Some("attribute") => {
                                // Attributes on memwr statements are not needed.
                                self.pos += 1;
                            }
                            _ => break,
                        }
                    }
                    p.syncs.push(sync);
                }
                Some("end") => {
                    self.pos += 1;
                    return Ok(p);
                }
                _ => {
                    let l = self.line()?;
                    return l.err("expected `sync` or `end` in process");
                }
            }
        }
    }
}

/// Extracts `width` bits starting at bit `start` from a chunk list.
fn slice_chunks(chunks: &[SigChunk], start: usize, width: usize) -> Option<Vec<SigChunk>> {
    let mut out = Vec::new();
    let mut pos = 0;
    let end = start + width;
    for c in chunks {
        let w = c.width();
        let (lo, hi) = (start.max(pos), end.min(pos + w));
        if lo < hi {
            out.push(match c {
                SigChunk::Wire { wire, offset, .. } => {
                    SigChunk::Wire { wire: wire.clone(), offset: offset + lo - pos, width: hi - lo }
                }
                SigChunk::Const(bits) => SigChunk::Const(bits[lo - pos..hi - pos].to_vec()),
            });
        }
        pos += w;
    }
    (end <= pos).then_some(out)
}

/// Parses an RTLIL file.
pub fn parse(text: &str) -> Result<Design, ParseError> {
    Parser::new(text)?.design()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# comment
autoidx 5
attribute \top 1
module \top
  parameter \P 8
  attribute \src "t.v:1"
  wire width 4 input 1 \a
  wire width 8 offset 4 \w
  wire width 4 upto \u
  wire output 2 \y
  cell \SB_LUT4 $lut$1
    parameter \LUT_INIT 16'1010101010101010
    connect \I0 \a [0]
    connect \I1 { \a [3:2] }
    connect \I2 1'x
    connect \O \y
  end
  connect \w [11:8] \a
  connect \u [0] \a [1]
  process $proc
    assign \w [7:4] 4'0011
    switch \a
      case 4'0001 , 4'0010
        assign \w [7:4] \a
      case
    end
    sync posedge \a [0]
      update \w [7:4] \w [11:8]
  end
end
"#;

    #[test]
    fn parse_sample() {
        let d = parse(SAMPLE).unwrap();
        let m = d.top().unwrap();
        assert_eq!(m.name, "top");
        assert_eq!(m.wires.len(), 4);
        assert_eq!(m.wires[0].port, Some((PortDir::Input, 1)));
        assert_eq!(m.wires[0].attrs[0].0, "src");
        let c = &m.cells[0];
        assert_eq!(c.kind, "SB_LUT4");
        assert_eq!(c.params[0].1.as_u64(), Some(0xAAAA));
        assert_eq!(
            c.connections[1].1.chunks,
            vec![SigChunk::Wire { wire: "a".into(), offset: 2, width: 2 }]
        );
        // `w` has offset 4, so [11:8] is storage bits 4..8.
        assert_eq!(
            m.connections[0].0.chunks,
            vec![SigChunk::Wire { wire: "w".into(), offset: 4, width: 4 }]
        );
        // `u` is upto: index 0 is the most significant storage bit.
        assert_eq!(
            m.connections[1].0.chunks,
            vec![SigChunk::Wire { wire: "u".into(), offset: 3, width: 1 }]
        );
        let p = &m.processes[0];
        assert_eq!(p.root.switches[0].cases.len(), 2);
        assert_eq!(p.root.switches[0].cases[0].compare.len(), 2);
        assert!(p.root.switches[0].cases[1].compare.is_empty());
        assert!(matches!(p.syncs[0].kind, SyncKind::Posedge(_)));
    }

    #[test]
    fn concat_order() {
        let text = "module \\m\n wire width 2 \\a\n wire \\b\n wire width 3 \\c\n connect \\c { \\b \\a }\nend\n";
        let d = parse(text).unwrap();
        // { b a }: a is the least significant part.
        assert_eq!(
            d.modules[0].connections[0].1.chunks,
            vec![
                SigChunk::Wire { wire: "a".into(), offset: 0, width: 2 },
                SigChunk::Wire { wire: "b".into(), offset: 0, width: 1 },
            ]
        );
    }

    #[test]
    fn signed_constants_sign_extend() {
        // Yosys writes signed parameters such as `32'sx` in cell models.
        let text = "module \\m\n cell \\C \\c\n  parameter signed \\P 4's10\n  parameter \\Q 4'10\n end\nend\n";
        let d = parse(text).unwrap();
        let params = &d.modules[0].cells[0].params;
        assert_eq!(params[0].1.as_u64(), Some(0b1110));
        assert_eq!(params[1].1.as_u64(), Some(0b0010));
    }

    #[test]
    fn errors_have_lines() {
        let e = parse("module \\m\n connect \\nope \\nope\nend\n").unwrap_err();
        assert_eq!(e.line, 2);
    }
}
