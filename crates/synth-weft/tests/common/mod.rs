//! Test helpers: compiling Weft sources and a small cycle-based RTL
//! simulator (every clock edge is the same rising edge).
#![allow(dead_code)]

use std::collections::HashMap;

use synth_core::{Logic, LogicVec, SourceMap};
use synth_rtl::{CellId, CellKind, MemId, Module, Sig, SigBit, eval};

/// Compiles Weft source text; returns the module or the rendered diagnostics.
pub fn compile(src: &str) -> Result<(Module, String), String> {
    compile_with(src, None, &[])
}

pub fn compile_with(
    src: &str,
    top: Option<&str>,
    params: &[(&str, &str)],
) -> Result<(Module, String), String> {
    let mut sm = SourceMap::new();
    let f = sm.add("test.weft", src);
    let opts = synth_weft::Options {
        top: top.map(String::from),
        params: params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    };
    let (m, diags) = synth_weft::compile(&mut sm, &[f], &[], None, &opts);
    let text = diags.render(&sm);
    match m {
        Some(m) => Ok((m, text)),
        None => Err(text),
    }
}

/// The diagnostics of a source that must fail to compile.
pub fn errors(src: &str) -> String {
    match compile(src) {
        Ok((_, w)) => panic!("expected errors, compiled with:\n{w}"),
        Err(e) => e,
    }
}

/// The warnings of a source that must compile.
pub fn warnings(src: &str) -> String {
    match compile(src) {
        Ok((_, w)) => w,
        Err(e) => panic!("expected success, got:\n{e}"),
    }
}

pub fn ok(src: &str) -> Module {
    match compile(src) {
        Ok((m, _)) => m,
        Err(e) => panic!("expected success, got:\n{e}"),
    }
}

#[derive(Clone, Copy)]
enum Node {
    Cell(usize),
    Conn(usize),
    Read(usize, usize),
}

pub struct Sim {
    pub m: Module,
    base: Vec<usize>,
    vals: Vec<Logic>,
    order: Vec<Node>,
    mems: Vec<Vec<LogicVec>>,
}

impl Sim {
    pub fn new(mut m: Module) -> Sim {
        synth_rtl::opt::clean(&mut m).unwrap();
        let mut base = Vec::new();
        let mut n = 0;
        for w in m.wires.iter() {
            base.push(n);
            n += w.width as usize;
        }
        let idx = |b: &SigBit| match b {
            SigBit::Wire(w, i) => Some(base[w.index()] + *i as usize),
            _ => None,
        };
        // Combinational drivers of each bit.
        let mut driver: HashMap<usize, Node> = HashMap::new();
        let mut deps: Vec<(Node, Vec<usize>)> = Vec::new();
        for (ci, c) in m.cells.iter().enumerate() {
            if !c.alive || matches!(c.kind, CellKind::Dff(_)) {
                continue;
            }
            let mut ins = Vec::new();
            c.for_each_input(|b| ins.extend(idx(&b)));
            for o in c.outputs() {
                for b in o.iter() {
                    if let Some(k) = idx(b) {
                        driver.insert(k, Node::Cell(ci));
                    }
                }
            }
            deps.push((Node::Cell(ci), ins));
        }
        for (k, (l, r)) in m.connections.iter().enumerate() {
            for b in l.iter() {
                if let Some(x) = idx(b) {
                    driver.insert(x, Node::Conn(k));
                }
            }
            deps.push((Node::Conn(k), r.iter().filter_map(idx).collect()));
        }
        for (mi, mem) in m.memories.iter().enumerate() {
            if !mem.alive {
                continue;
            }
            for (ri, r) in mem.reads.iter().enumerate() {
                if r.clk.is_some() {
                    continue;
                }
                for b in r.data.iter() {
                    if let Some(x) = idx(b) {
                        driver.insert(x, Node::Read(mi, ri));
                    }
                }
                deps.push((Node::Read(mi, ri), r.addr.iter().filter_map(idx).collect()));
            }
        }
        // Topological order.
        let key = |n: &Node| match n {
            Node::Cell(c) => (0, *c, 0),
            Node::Conn(c) => (1, *c, 0),
            Node::Read(a, b) => (2, *a, *b),
        };
        let dep_of: HashMap<(i32, usize, usize), Vec<usize>> =
            deps.iter().map(|(n, d)| (key(n), d.clone())).collect();
        let mut state: HashMap<(i32, usize, usize), u8> = HashMap::new();
        let mut order = Vec::new();
        for (n, _) in &deps {
            let mut stack = vec![(*n, false)];
            while let Some((x, done)) = stack.pop() {
                let k = key(&x);
                if done {
                    state.insert(k, 2);
                    order.push(x);
                    continue;
                }
                if state.contains_key(&k) {
                    continue;
                }
                state.insert(k, 1);
                stack.push((x, true));
                for b in &dep_of[&k] {
                    if let Some(dn) = driver.get(b)
                        && !state.contains_key(&key(dn))
                    {
                        stack.push((*dn, false));
                    }
                }
            }
        }
        let mems = m.memories.iter().map(|mem| mem.init.clone()).collect();
        let mut s = Sim { m, base, vals: vec![Logic::X; n], order, mems };
        // Power-on values.
        for c in s.m.cells.clone().iter() {
            if let CellKind::Dff(d) = &c.kind {
                s.set(&d.q, &d.init);
            }
        }
        s.settle();
        s
    }

    fn get(&self, s: &Sig) -> LogicVec {
        let bits: Vec<Logic> = s
            .iter()
            .map(|b| match b {
                SigBit::Const(l) => *l,
                SigBit::Wire(w, i) => self.vals[self.base[w.index()] + *i as usize],
            })
            .collect();
        LogicVec::from_bits(&bits)
    }

    fn set(&mut self, s: &Sig, v: &LogicVec) {
        for (k, b) in s.iter().enumerate() {
            if let SigBit::Wire(w, i) = b {
                self.vals[self.base[w.index()] + *i as usize] = v.get(k);
            }
        }
    }

    fn settle(&mut self) {
        for k in 0..self.order.len() {
            match self.order[k] {
                Node::Conn(c) => {
                    let (l, r) = self.m.connections[c].clone();
                    let v = self.get(&r);
                    self.set(&l, &v);
                }
                Node::Read(mi, ri) => {
                    let r = &self.m.memories[MemId::new(mi)].reads[ri];
                    let (addr, data) = (r.addr.clone(), r.data.clone());
                    let a = self.get(&addr).to_u64();
                    let w = data.width();
                    let v = match a {
                        Some(a) if (a as usize) < self.mems[mi].len() => {
                            self.mems[mi][a as usize].clone()
                        }
                        _ => LogicVec::xs(w),
                    };
                    self.set(&data, &v);
                }
                Node::Cell(ci) => {
                    let kind = self.m.cells[CellId::new(ci)].kind.clone();
                    match kind {
                        CellKind::Unary { op, a, y } => {
                            let v = eval::unary(op, &self.get(&a));
                            self.set(&y, &v.resize(y.width(), false));
                        }
                        CellKind::Binary { op, signed, a, b, y } => {
                            let v = eval::binary(op, signed, &self.get(&a), &self.get(&b));
                            self.set(&y, &v.resize(y.width(), false));
                        }
                        CellKind::Mux { a, b, s, y, .. } => {
                            let sv = self.get(&Sig::bit(s)).get(0);
                            let v = eval::mux(&self.get(&a), &self.get(&b), sv);
                            self.set(&y, &v);
                        }
                        CellKind::Pmux { default, cases, s, y } => {
                            let sv = self.get(&s);
                            let ones: Vec<usize> =
                                (0..sv.width()).filter(|&i| sv.get(i) == Logic::One).collect();
                            let unknown = (0..sv.width()).any(|i| !sv.get(i).is_known());
                            let v = if unknown || ones.len() > 1 {
                                LogicVec::xs(y.width())
                            } else if let Some(&i) = ones.first() {
                                self.get(&cases[i])
                            } else {
                                self.get(&default)
                            };
                            self.set(&y, &v);
                        }
                        CellKind::Shiftx { a, b, b_signed, offset, y } => {
                            let v = eval::shiftx(
                                &self.get(&a),
                                &self.get(&b),
                                b_signed,
                                offset,
                                y.width(),
                            );
                            self.set(&y, &v);
                        }
                        CellKind::Instance(i) => {
                            for (_, dir, s) in &i.conns {
                                if *dir != synth_rtl::PortDir::Input {
                                    self.set(s, &LogicVec::xs(s.width()));
                                }
                            }
                        }
                        CellKind::Latch(_) | CellKind::Dff(_) => {}
                    }
                }
            }
        }
        // Asynchronous resets.
        let mut changed = false;
        for c in self.m.cells.clone().iter() {
            if let CellKind::Dff(d) = &c.kind
                && let Some((ctl, v)) = &d.arst
                && self.get(&Sig::bit(ctl.sig)).get(0) == Logic::from_bool(ctl.pol)
                && self.get(&d.q) != *v
            {
                self.set(&d.q, v);
                changed = true;
            }
        }
        if changed {
            self.settle();
        }
    }

    pub fn wire(&self, name: &str) -> Sig {
        let (i, w) = self
            .m
            .wires
            .iter_enumerated()
            .find(|(_, w)| w.name == name)
            .unwrap_or_else(|| panic!("no wire `{name}`"));
        Sig::wire(i, w.width)
    }

    pub fn poke(&mut self, name: &str, v: u64) {
        let s = self.wire(name);
        let w = s.width();
        self.set(&s, &LogicVec::from_u64(v, w));
        self.settle();
    }

    pub fn peek(&self, name: &str) -> Option<u64> {
        self.get(&self.wire(name)).to_u64()
    }

    pub fn peek_vec(&self, name: &str) -> LogicVec {
        self.get(&self.wire(name))
    }

    /// One rising clock edge.
    pub fn step(&mut self) {
        self.settle();
        let active = |s: &Sim, c: &synth_rtl::Control| {
            s.get(&Sig::bit(c.sig)).get(0) == Logic::from_bool(c.pol)
        };
        let mut next = Vec::new();
        for c in self.m.cells.iter() {
            let CellKind::Dff(d) = &c.kind else { continue };
            if !c.alive {
                continue;
            }
            let en = d.en.as_ref().is_none_or(|e| active(self, e));
            let v = if let Some((ctl, v)) = &d.arst
                && active(self, ctl)
            {
                v.clone()
            } else if let Some((ctl, v)) = &d.srst
                && active(self, ctl)
                && (!d.en_over_srst || en)
            {
                v.clone()
            } else if en {
                self.get(&d.d)
            } else {
                self.get(&d.q)
            };
            next.push((d.q.clone(), v));
        }
        let mut mem_next = Vec::new();
        let mut sync_reads = Vec::new();
        for (mi, mem) in self.m.memories.iter().enumerate() {
            if !mem.alive {
                continue;
            }
            for r in &mem.reads {
                if r.clk.is_some() && self.get(&Sig::bit(r.en)).get(0) == Logic::One {
                    let a = self.get(&r.addr).to_u64();
                    let v = match a {
                        Some(a) if (a as usize) < self.mems[mi].len() => {
                            self.mems[mi][a as usize].clone()
                        }
                        _ => LogicVec::xs(r.data.width()),
                    };
                    sync_reads.push((r.data.clone(), v));
                }
            }
            for w in &mem.writes {
                let en = self.get(&w.en);
                let Some(a) = self.get(&w.addr).to_u64() else { continue };
                let data = self.get(&w.data);
                mem_next.push((mi, a as usize, en, data));
            }
        }
        for (mi, a, en, data) in mem_next {
            if a < self.mems[mi].len() {
                for b in 0..en.width() {
                    if en.get(b) == Logic::One {
                        self.mems[mi][a].set(b, data.get(b));
                    }
                }
            }
        }
        for (q, v) in next {
            self.set(&q, &v);
        }
        for (d, v) in sync_reads {
            self.set(&d, &v);
        }
        self.settle();
    }
}

/// Compiles files from the repository (paths relative to its root), with
/// Verilog files for extern modules.
pub fn compile_files(
    weft: &[&str],
    verilog: &[&str],
    top: Option<&str>,
    params: &[(&str, &str)],
) -> Module {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut sm = SourceMap::new();
    let mut ids = Vec::new();
    for p in weft {
        let path = root.join(p);
        ids.push(sm.add(path.display().to_string(), std::fs::read_to_string(&path).unwrap()));
    }
    let src = if verilog.is_empty() {
        None
    } else {
        let vids: Vec<_> = verilog
            .iter()
            .map(|p| {
                let path = root.join(p);
                sm.add(path.display().to_string(), std::fs::read_to_string(&path).unwrap())
            })
            .collect();
        let (src, d) = synth_verilog::parse_files(&mut sm, &vids, &Default::default());
        assert!(!d.has_errors(), "{}", d.render(&sm));
        Some(src)
    };
    let opts = synth_weft::Options {
        top: top.map(String::from),
        params: params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    };
    let (m, diags) = synth_weft::compile(&mut sm, &ids, &[], src.as_ref(), &opts);
    let text = diags.render(&sm);
    assert!(!text.contains("warning"), "examples compile without warnings:\n{text}");
    m.unwrap_or_else(|| panic!("{text}"))
}

/// Elaborates Verilog files (for differential tests).
pub fn verilog(files: &[&str], top: &str) -> Module {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut sm = SourceMap::new();
    let ids: Vec<_> = files
        .iter()
        .map(|p| {
            let path = root.join(p);
            sm.add(path.display().to_string(), std::fs::read_to_string(&path).unwrap())
        })
        .collect();
    let (src, d) = synth_verilog::parse_files(&mut sm, &ids, &Default::default());
    assert!(!d.has_errors(), "{}", d.render(&sm));
    let opts = synth_elab::ElabOptions { top: Some(top.into()), top_params: vec![] };
    let (m, d) = synth_elab::elaborate(&src, &sm, &opts, &synth_elab::NoBlackBoxes);
    m.unwrap_or_else(|| panic!("{}", d.render(&sm)))
}

/// Decodes 8N1 bytes from a sampled serial line.
pub fn uart_decode(line: &[u64], clks_per_bit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < line.len() {
        if line[i - 1] == 1 && line[i] == 0 {
            let mid = i + clks_per_bit / 2;
            if mid + 9 * clks_per_bit >= line.len() {
                break;
            }
            let mut b = 0u8;
            for k in 0..8 {
                b |= (line[mid + (k + 1) * clks_per_bit] as u8) << k;
            }
            out.push(b);
            i = mid + 9 * clks_per_bit;
        } else {
            i += 1;
        }
    }
    out
}
