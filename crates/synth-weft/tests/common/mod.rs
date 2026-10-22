//! Test helpers: compiling Weft sources and simulating them.
#![allow(dead_code)]

use synth_core::{LogicVec, SourceMap};
use synth_rtl::Module;

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
        ..Default::default()
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

/// A simulation through `synth-sim`, where `step` is one cycle of every
/// clock (all start low).
pub struct Sim {
    pub sim: synth_sim::Sim,
    clocks: Vec<synth_sim::Signal>,
}

impl Sim {
    pub fn new(m: Module) -> Sim {
        let mut sim = synth_sim::Sim::new(&m, &synth_sim::Options::default()).unwrap();
        let clocks: Vec<_> = sim.clocks().into_iter().filter(|c| sim.writable(c)).collect();
        for c in &clocks {
            sim.poke(c, 0);
        }
        sim.eval_quiet();
        Sim { sim, clocks }
    }

    fn signal(&self, name: &str) -> synth_sim::Signal {
        self.sim.signal(name).unwrap_or_else(|| panic!("no wire `{name}`"))
    }

    pub fn poke(&mut self, name: &str, v: u64) {
        let s = self.signal(name);
        self.sim.poke(&s, v);
        self.sim.eval();
    }

    pub fn peek(&self, name: &str) -> Option<u64> {
        self.sim.peek(&self.signal(name))
    }

    pub fn peek_vec(&self, name: &str) -> LogicVec {
        self.sim.peek_vec(&self.signal(name))
    }

    /// One clock cycle: every clock rises, then falls.
    pub fn step(&mut self) {
        for v in [1, 0] {
            for c in &self.clocks {
                self.sim.poke(c, v);
            }
            self.sim.eval();
        }
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
        ..Default::default()
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
