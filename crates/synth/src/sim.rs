//! `synth sim`: simulates a design with synth-sim.

use std::io::Write;
use std::time::Instant;

use synth_core::LogicVec;
use synth_sim::{Options, Sim, WritePort, attach_memory, emit_c, vcd::Vcd};

use crate::args::Args;

/// Parses a value: decimal, `0x` hexadecimal or `0b` binary.
fn parse_value(v: &str) -> Result<u64, String> {
    let v = v.replace('_', "");
    let r = if let Some(h) = v.strip_prefix("0x") {
        u64::from_str_radix(h, 16)
    } else if let Some(b) = v.strip_prefix("0b") {
        u64::from_str_radix(b, 2)
    } else {
        v.parse()
    };
    r.map_err(|_| format!("bad value `{v}`"))
}

fn assignment(s: &str) -> Result<(String, u64), String> {
    let (n, v) = s.split_once('=').ok_or_else(|| format!("expected NAME=VALUE, got `{s}`"))?;
    Ok((n.to_string(), parse_value(v)?))
}

/// Words of a `$readmemh`-style file: hexadecimal words separated by
/// white space, `//` comments, and `@address` jumps.
fn read_hex(text: &str) -> Result<Vec<(i64, u64)>, String> {
    let mut out = Vec::new();
    let mut addr = 0i64;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("");
        for tok in line.split_whitespace() {
            if let Some(a) = tok.strip_prefix('@') {
                addr = i64::from_str_radix(a, 16).map_err(|_| format!("bad address `{tok}`"))?;
                continue;
            }
            let v = u64::from_str_radix(&tok.replace('_', ""), 16)
                .map_err(|_| format!("bad hexadecimal word `{tok}`"))?;
            out.push((addr, v));
            addr += 1;
        }
    }
    Ok(out)
}

/// An 8N1 serial receiver sampling a line once per cycle.
struct Uart {
    sig: synth_sim::Signal,
    clocks_per_bit: u64,
    prev: u64,
    /// Cycle of the start bit's falling edge, while receiving.
    start: Option<u64>,
    byte: u8,
}

impl Uart {
    /// Samples the line at `cycle`; returns a completed byte.
    fn sample(&mut self, sim: &Sim, cycle: u64) -> Option<u8> {
        let v = sim.peek(&self.sig).unwrap_or(1) & 1;
        let prev = std::mem::replace(&mut self.prev, v);
        let cpb = self.clocks_per_bit;
        match self.start {
            None => {
                if prev == 1 && v == 0 {
                    self.start = Some(cycle);
                    self.byte = 0;
                }
                None
            }
            Some(s) => {
                let t = cycle - s;
                let mid = cpb / 2;
                if t > mid && (t - mid).is_multiple_of(cpb) {
                    let k = (t - mid) / cpb;
                    if (1..=8).contains(&k) {
                        self.byte |= (v as u8) << (k - 1);
                    } else if k == 9 {
                        self.start = None;
                        return Some(self.byte);
                    }
                }
                None
            }
        }
    }
}

/// The design after iCE40 synthesis, as RTL built from the primitives'
/// simulation models.
pub fn synthesized(m: synth_rtl::Module) -> Result<synth_rtl::Module, String> {
    let (nl, _) = arch_ice40::synth::synthesize(m, &arch_ice40::synth::SynthOptions::default())?;
    synth_sim::netlist::from_netlist(&nl, &mut arch_ice40::sim::add_cell)
}

pub fn cmd_sim(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(
        argv,
        &[
            "-I",
            "-D",
            "-P",
            "--top",
            "--clock",
            "--cycles",
            "--until",
            "--set",
            "--at",
            "--print",
            "--vcd",
            "--uart",
            "--load",
            "--attach",
            "--emit-c",
            "--compare",
            "--reset",
            "--reset-low",
            "--hold",
            "--seed",
            "--device",
            "--package",
            "--pcf",
            "--bitstream",
            "--freq",
            "--timing-weight",
            "--level",
        ],
        &["--four-state", "--two-state", "--trace", "--quiet", "--optimize", "--stats"],
    )?;
    let weft = crate::input_files(&a).iter().any(|p| crate::is_weft(p));
    let mut m = crate::load_rtl(&mut a)?;
    if let Some(what) = a.value("--compare") {
        return compare(&mut a, m, &what);
    }
    let level = a.value("--level").unwrap_or_else(|| "rtl".into());
    m = match level.as_str() {
        "rtl" => m,
        "synth" => synthesized(m)?,
        "bitstream" => bitstream(&mut a, m)?,
        _ => return Err(format!("--level expects rtl, synth or bitstream, got `{level}`")),
    };
    for spec in a.values("--attach") {
        let f: Vec<&str> = spec.split(':').collect();
        let bad =
            || format!("--attach expects NAME:ADDR:DATA:DEPTH[:CLOCK:ENABLE:WDATA], got `{spec}`");
        if f.len() != 4 && f.len() != 7 {
            return Err(bad());
        }
        let depth = parse_value(f[3])? as u32;
        let write = (f.len() == 7).then(|| WritePort {
            clock: f[4],
            posedge: true,
            enable: f[5],
            data: f[6],
            addr: None,
        });
        attach_memory(&mut m, f[0], f[1], f[2], depth, write)?;
    }
    let four = if a.flag("--four-state") {
        true
    } else if a.flag("--two-state") {
        false
    } else {
        // Weft is two-state, and so is hardware; Verilog RTL keeps x as
        // Icarus does.
        !weft && level == "rtl"
    };
    if let Some(dir) = a.value("--emit-c") {
        // The model shows only ports and memories: always optimized.
        let sim = Sim::new(&m, &Options { four_state: false, prune: false, optimize: true })?;
        let model = emit_c::emit(sim.program(), &m.name)?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
        let h = format!("{dir}/{}.h", model.prefix);
        let c = format!("{dir}/{}.c", model.prefix);
        crate::write(&h, model.header.as_bytes())?;
        crate::write(&c, model.source.as_bytes())?;
        eprintln!("info: wrote {h} and {c}");
        return Ok(());
    }
    let quiet = a.flag("--quiet");
    let t0 = Instant::now();
    let optimize = a.flag("--optimize");
    let mut sim = Sim::new(&m, &Options { four_state: four, prune: false, optimize })?;
    for w in sim.warnings() {
        eprintln!("warning: {w}");
    }
    if !quiet {
        eprintln!(
            "info: compiled in {:.2?}: {} instructions, {} state words, {}-state",
            t0.elapsed(),
            sim.program().ops.len(),
            sim.program().init_val.len(),
            if four { 4 } else { 2 }
        );
    }
    for spec in a.values("--load") {
        let (name, file) = spec
            .split_once('=')
            .ok_or_else(|| format!("--load expects MEMORY=FILE, got `{spec}`"))?;
        let mem = sim.memory(name).ok_or_else(|| {
            let names: Vec<&str> = sim.memory_names().collect();
            format!("no memory `{name}` (memories: {})", names.join(", "))
        })?;
        let (width, _, offset) = sim.memory_shape(mem);
        for (addr, v) in read_hex(&crate::read(file)?)? {
            sim.write_memory(mem, offset + addr, &LogicVec::from_u64(v, width as usize));
        }
    }
    let signal = |sim: &Sim, n: &str| sim.signal(n).ok_or_else(|| format!("no signal `{n}`"));
    let clock_name = a.value("--clock");
    let clk = match &clock_name {
        Some(n) => Some(signal(&sim, n)?),
        None => sim.signal("clk"),
    };
    for s in a.values("--set") {
        let (n, v) = assignment(&s)?;
        let sig = signal(&sim, &n)?;
        sim.try_poke_vec(
            &sig,
            &LogicVec::from_u64(v, sig.width().max(1)).resize(sig.width(), false),
        )
        .map_err(|e| format!("--set {n}: {e}"))?;
    }
    let mut at: Vec<(u64, synth_sim::Signal, u64)> = Vec::new();
    for s in a.values("--at") {
        let (c, asg) =
            s.split_once(':').ok_or_else(|| format!("--at expects CYCLE:NAME=VALUE, got `{s}`"))?;
        let (n, v) = assignment(asg)?;
        at.push((parse_value(c)?, signal(&sim, &n)?, v));
    }
    at.sort_by_key(|x| x.0);
    let until = match a.value("--until") {
        Some(u) => {
            let (n, v) = match u.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(parse_value(v)?)),
                None => (u.clone(), None),
            };
            Some((signal(&sim, &n)?, v))
        }
        None => None,
    };
    let cycles = match a.value("--cycles") {
        Some(c) => parse_value(&c)?,
        None if until.is_some() => u64::MAX,
        None => 1000,
    };
    let prints: Vec<(String, synth_sim::Signal)> = a
        .values("--print")
        .iter()
        .map(|n| Ok((n.clone(), signal(&sim, n)?)))
        .collect::<Result<_, String>>()?;
    let trace = a.flag("--trace");
    let a_stats = a.flag("--stats");
    let mut uart = match a.value("--uart") {
        Some(spec) => {
            let (n, cpb) = match spec.split_once(':') {
                Some((n, c)) => (n.to_string(), parse_value(c)?),
                None => (spec.clone(), 1),
            };
            Some(Uart {
                sig: signal(&sim, &n)?,
                clocks_per_bit: cpb.max(1),
                prev: 1,
                start: None,
                byte: 0,
            })
        }
        None => None,
    };
    let mut vcd = match a.value("--vcd") {
        Some(path) => {
            let f = std::fs::File::create(&path).map_err(|e| format!("{path}: {e}"))?;
            let w = std::io::BufWriter::new(f);
            Some(
                Vcd::new(&sim, w, &m.name, "1ns", |n| !n.contains('$'))
                    .map_err(|e| format!("{path}: {e}"))?,
            )
        }
        None => None,
    };
    a.finish()?;

    let show = |sim: &Sim, sig: &synth_sim::Signal| -> String {
        let v = sim.peek_vec(sig);
        match v.to_u64() {
            Some(x) => format!("{x} (0x{x:x})"),
            None => v.to_bin_string(),
        }
    };
    if let Some(c) = &clk {
        sim.poke(c, 0);
        sim.eval_quiet();
    } else {
        sim.eval();
    }
    let vcd_err = |e: std::io::Error| format!("VCD: {e}");
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut next_at = 0;
    let t1 = Instant::now();
    let mut cycle = 0u64;
    let mut stopped = false;
    while cycle < cycles {
        while next_at < at.len() && at[next_at].0 <= cycle {
            let (_, sig, v) = &at[next_at];
            sim.poke(sig, *v);
            next_at += 1;
        }
        sim.eval();
        if let Some(v) = &mut vcd {
            v.sample(&sim, cycle * 10).map_err(vcd_err)?;
        }
        if let Some((sig, want)) = &until {
            let hit = match (sim.peek(sig), want) {
                (Some(x), Some(w)) => x == *w,
                (Some(x), None) => x != 0,
                _ => false,
            };
            if hit {
                stopped = true;
                break;
            }
        }
        let Some(c) = &clk else {
            cycle += 1;
            continue;
        };
        sim.poke(c, 1);
        sim.eval();
        if let Some(v) = &mut vcd {
            v.sample(&sim, cycle * 10 + 5).map_err(vcd_err)?;
        }
        sim.poke(c, 0);
        sim.eval();
        cycle += 1;
        if let Some(u) = &mut uart
            && let Some(b) = u.sample(&sim, cycle)
        {
            out.write_all(&[b]).and_then(|_| out.flush()).map_err(|e| e.to_string())?;
        }
        if trace {
            let vals: Vec<String> =
                prints.iter().map(|(n, s)| format!("{n}={}", show(&sim, s))).collect();
            writeln!(out, "{cycle}: {}", vals.join(" ")).map_err(|e| e.to_string())?;
        }
    }
    let el = t1.elapsed();
    if let Some(v) = vcd {
        v.finish(cycle * 10).map_err(vcd_err)?;
    }
    if !trace {
        for (n, s) in &prints {
            writeln!(out, "{n} = {}", show(&sim, s)).map_err(|e| e.to_string())?;
        }
    }
    if let Some((_, _)) = &until
        && !stopped
    {
        return Err(format!("the --until condition did not hold within {cycles} cycles"));
    }
    if !quiet {
        eprintln!(
            "info: {cycle} cycles in {el:.2?} ({:.0} cycles/s){}",
            cycle as f64 / el.as_secs_f64().max(1e-9),
            if stopped { ", stopped by --until" } else { "" }
        );
    }
    if a_stats {
        let p = sim.program();
        let mut kinds = std::collections::BTreeMap::<String, usize>::new();
        for o in &p.ops {
            *kinds.entry(format!("{:?}", o.k)).or_default() += 1;
        }
        let mut seq = 0;
        for d in &p.domains {
            seq += d.sample.len() + d.write.len() + d.commit.len() + d.gated.len();
        }
        eprintln!("info: instructions: {kinds:?}; {seq} in clock domains");
        eprintln!(
            "info: {:.1} combinational units run per cycle, {} edges",
            sim.units_run() as f64 / cycle.max(1) as f64,
            sim.edges
        );
    }
    if sim.unstable_loops() > 0 {
        eprintln!("warning: {} combinational loops did not settle", sim.unstable_loops());
    }
    Ok(())
}

/// `--compare synth`: the RTL (two-state constants and initial values,
/// four-state simulation) against its iCE40 netlist under random inputs,
/// comparing every output bit the RTL defines after every rising edge.
fn compare(a: &mut Args, m: synth_rtl::Module, what: &str) -> Result<(), String> {
    let mut reference = m.clone();
    synth_rtl::opt::two_state(&mut reference);
    let other = match what {
        "synth" => synthesized(m)?,
        "bitstream" => bitstream(a, m)?,
        _ => return Err(format!("--compare expects `synth` or `bitstream`, got `{what}`")),
    };
    let clocks = {
        let c = a.values("--clock");
        if c.is_empty() { vec!["clk".to_string()] } else { c }
    };
    let reset = a.value("--reset");
    let reset_low = a.value("--reset-low");
    let cycles = a.value("--cycles").map(|c| parse_value(&c)).transpose()?.unwrap_or(10_000);
    let hold = a.value("--hold").map(|c| parse_value(&c)).transpose()?.unwrap_or(20).max(1);
    let seed = a.value("--seed").map(|c| parse_value(&c)).transpose()?.unwrap_or(1);
    let quiet = a.flag("--quiet");
    // Inputs held at a value instead of random ones.
    let held: Vec<(String, u64)> =
        a.values("--set").iter().map(|s| assignment(s)).collect::<Result<_, _>>()?;
    a.finish()?;
    let mut r = Sim::new(&reference, &Options { four_state: true, prune: false, optimize: false })?;
    let mut n = Sim::new(&other, &Options { four_state: false, prune: false, optimize: false })?;
    for w in n.warnings() {
        eprintln!("warning: {w}");
    }
    let find = |sim: &Sim, name: &str| {
        sim.signal(name).ok_or_else(|| format!("no port `{name}` in the {what} netlist"))
    };
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for &(si, dir, _) in &r.program().ports {
        let name = r.program().signals[si].0.clone();
        let rs = r.signal(&name).unwrap();
        match dir {
            synth_rtl::PortDir::Input => {
                if !clocks.contains(&name) {
                    inputs.push((name.clone(), rs, find(&n, &name)?));
                }
            }
            _ => outputs.push((name.clone(), rs, find(&n, &name)?)),
        }
    }
    let clk: Vec<_> = clocks
        .iter()
        .map(|c| Ok((signal_of(&r, c)?, find(&n, c)?)))
        .collect::<Result<_, String>>()?;
    let mut rng = synth_core::Rng::new(seed);
    let set =
        |r: &mut Sim, n: &mut Sim, rs: &synth_sim::Signal, ns: &synth_sim::Signal, v: &LogicVec| {
            r.poke_vec(rs, v);
            n.poke_vec(ns, v);
        };
    // Inputs start at 0 (or their --set value), resets active.
    inputs.retain(|(name, rs, ns)| {
        let Some((_, v)) = held.iter().find(|(n, _)| n == name) else { return true };
        let v = LogicVec::from_u64(*v, rs.width().max(1)).resize(rs.width(), false);
        r.poke_vec(rs, &v);
        n.poke_vec(ns, &v);
        false
    });
    for (name, rs, ns) in &inputs {
        let w = rs.width();
        let v =
            if Some(name) == reset_low.as_ref() { LogicVec::ones(w) } else { LogicVec::zeros(w) };
        let v = if Some(name) == reset.as_ref() {
            LogicVec::ones(w)
        } else if Some(name) == reset_low.as_ref() {
            LogicVec::zeros(w)
        } else {
            v
        };
        set(&mut r, &mut n, rs, ns, &v);
    }
    for (rc, nc) in &clk {
        set(&mut r, &mut n, rc, nc, &LogicVec::zeros(1));
    }
    r.eval_quiet();
    n.eval_quiet();
    let mut errors = 0u64;
    let t = Instant::now();
    for cycle in 0..cycles {
        for (rc, nc) in &clk {
            set(&mut r, &mut n, rc, nc, &LogicVec::from_bool(true));
        }
        r.eval();
        n.eval();
        for (name, rs, ns) in &outputs {
            let (rv, nv) = (r.peek_vec(rs), n.peek_vec(ns));
            let differs = (0..rv.width()).any(|i| rv.get(i).is_known() && rv.get(i) != nv.get(i));
            if differs {
                errors += 1;
                if errors <= 10 {
                    eprintln!("cycle {cycle}: {name} reference {rv:?} {what} {nv:?}");
                }
            }
        }
        for (rc, nc) in &clk {
            set(&mut r, &mut n, rc, nc, &LogicVec::zeros(1));
        }
        for (name, rs, ns) in &inputs {
            if Some(name) == reset.as_ref() || Some(name) == reset_low.as_ref() {
                if cycle == 4 {
                    let v = LogicVec::from_bool(reset_low.as_ref() == Some(name));
                    set(&mut r, &mut n, rs, ns, &v.resize(rs.width(), false));
                }
                continue;
            }
            if rng.below(1024) < (1024 / hold as usize).max(1) {
                let w = rs.width();
                let words: Vec<u64> = (0..w.div_ceil(64).max(1)).map(|_| rng.next_u64()).collect();
                set(&mut r, &mut n, rs, ns, &LogicVec::from_planes(w, &words, &[]));
            }
        }
        r.eval();
        n.eval();
    }
    if errors > 0 {
        return Err(format!("{errors} mismatches in {cycles} cycles"));
    }
    if !quiet {
        eprintln!("info: compared in {:.2?}", t.elapsed());
    }
    println!("PASS {cycles} cycles");
    Ok(())
}

/// What the iCE40 bitstream of the design (built, or read from
/// --bitstream) implements, decoded back to cells, as RTL.
fn bitstream(a: &mut Args, m: synth_rtl::Module) -> Result<synth_rtl::Module, String> {
    let part: arch_ice40::arch::Part = a.required("--device")?.parse()?;
    let package = a.required("--package")?;
    let pcf = arch_ice40::pcf::parse_pcf(&crate::read(&a.required("--pcf")?)?)?;
    let cfg = match a.value("--bitstream") {
        Some(path) => {
            let data = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
            arch_ice40::bitstream::Config::from_bin(&data)
                .map_err(|e| format!("{path}: {}", e.0))?
        }
        None => {
            let (nl, _) =
                arch_ice40::synth::synthesize(m, &arch_ice40::synth::SynthOptions::default())?;
            let opts = arch_ice40::flow::FlowOptions {
                part,
                package: package.clone(),
                seed: 1,
                place_effort: 1.0,
                timing_weight: crate::timing_weight(a)?,
                clocks: crate::clocks(a, &pcf)?,
                route_timing_rounds: 0,
            };
            arch_ice40::flow::implement(nl, &pcf.pins, &opts)?.config
        }
    };
    let arch = arch_ice40::arch::Arch::new(part, &package)?;
    let (nl, warnings) = arch_ice40::decode::decode(&cfg, &arch, &pcf.pins)?;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    synth_sim::netlist::from_netlist(&nl, &mut arch_ice40::sim::add_cell)
}

fn signal_of(sim: &Sim, name: &str) -> Result<synth_sim::Signal, String> {
    sim.signal(name).ok_or_else(|| format!("no signal `{name}`"))
}
