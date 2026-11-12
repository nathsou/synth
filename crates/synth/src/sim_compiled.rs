//! `synth sim --compiled`: the simulation as a C model with a driver made
//! for this run, built by the C compiler and run.
//!
//! The driver does what the interpreter's loop in `sim.rs` does (inputs by
//! --set and --at, cycles of the clock, --until, --print, --trace, --uart,
//! --load) and prints the same output. The program is two-state and
//! optimized as `--emit-c` makes it, so only signals that remain after the
//! RTL optimizations can be named.

use std::fmt::Write as _;
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use synth_sim::program::Program;
use synth_sim::{Options, Sim, emit_c};

use crate::args::Args;
use crate::sim::{assignment, parse_value, read_hex};

/// The C model's prefix.
const PREFIX: &str = "model";

/// A C string literal.
fn c_str(s: &str) -> String {
    let mut out = String::from("\"");
    for b in s.bytes() {
        match b {
            b'"' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => write!(out, "\\{b:03o}").unwrap(),
        }
    }
    out.push('"');
    out
}

/// The first signal named `name`, as `Sim::signal` finds it.
fn find(p: &Program, name: &str) -> Result<usize, String> {
    p.signals.iter().position(|(n, _)| n == name).ok_or_else(|| format!("no signal `{name}`"))
}

fn poke(si: usize, v: u64) -> String {
    format!("{PREFIX}_poke{si}(&m, {v}ull);")
}

fn peek(si: usize) -> String {
    format!("{PREFIX}_peek{si}(&m)")
}

/// A temporary directory, removed when dropped.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn run(a: &mut Args, m: &synth_rtl::Module) -> Result<(), String> {
    for (opt, why) in [
        ("--vcd", "writes no VCD"),
        ("--emit-c", "and --emit-c are alternatives"),
        ("--four-state", "is two-state"),
    ] {
        if a.flag(opt) || a.value(opt).is_some() {
            return Err(format!("--compiled {why} ({opt})"));
        }
    }
    let quiet = a.flag("--quiet");
    let t0 = Instant::now();
    let sim = Sim::new(m, &Options { four_state: false, prune: false, optimize: true })?;
    for w in sim.warnings() {
        eprintln!("warning: {w}");
    }
    let p = sim.program();

    // What the run reads and writes.
    let mut observed: Vec<usize> = Vec::new();
    let mut observe = |si: usize| {
        if !observed.contains(&si) {
            observed.push(si);
        }
        si
    };
    let writable = |si: usize, what: &str| {
        if emit_c::writable(p, si) {
            Ok(si)
        } else {
            Err(format!("{what}: the signal is driven by logic or constant"))
        }
    };
    let clk = match a.value("--clock") {
        Some(n) => Some(observe(writable(find(p, &n)?, &format!("--clock {n}"))?)),
        None => find(p, "clk").ok().filter(|&si| emit_c::writable(p, si)).map(&mut observe),
    };
    let mut sets = Vec::new();
    for s in a.values("--set") {
        let (n, v) = assignment(&s)?;
        sets.push((observe(writable(find(p, &n)?, &format!("--set {n}"))?), v));
    }
    let mut at: Vec<(u64, usize, u64)> = Vec::new();
    for s in a.values("--at") {
        let (c, asg) =
            s.split_once(':').ok_or_else(|| format!("--at expects CYCLE:NAME=VALUE, got `{s}`"))?;
        let (n, v) = assignment(asg)?;
        at.push((parse_value(c)?, observe(writable(find(p, &n)?, &format!("--at {n}"))?), v));
    }
    at.sort_by_key(|x| x.0);
    let until = match a.value("--until") {
        Some(u) => {
            let (n, v) = match u.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(parse_value(v)?)),
                None => (u.clone(), None),
            };
            Some((observe(find(p, &n)?), v))
        }
        None => None,
    };
    let cycles = match a.value("--cycles") {
        Some(c) => parse_value(&c)?,
        None if until.is_some() => u64::MAX,
        None => 1000,
    };
    let prints: Vec<(String, usize)> = a
        .values("--print")
        .into_iter()
        .map(|n| Ok((n.clone(), observe(find(p, &n)?))))
        .collect::<Result<_, String>>()?;
    let uart = match a.value("--uart") {
        Some(spec) => {
            let (n, cpb) = match spec.split_once(':') {
                Some((n, c)) => (n.to_string(), parse_value(c)?),
                None => (spec.clone(), 1),
            };
            Some((observe(find(p, &n)?), cpb.max(1)))
        }
        None => None,
    };
    let trace = a.flag("--trace");
    let stats = a.flag("--stats");
    // Memory contents, as (memory, word, value) for the driver to load.
    let mut load = Vec::new();
    for spec in a.values("--load") {
        let (name, file) = spec
            .split_once('=')
            .ok_or_else(|| format!("--load expects MEMORY=FILE, got `{spec}`"))?;
        let Some(mi) = sim.memory_names().position(|n| n == name) else {
            let names: Vec<&str> = sim.memory_names().collect();
            return Err(format!("no memory `{name}` (memories: {})", names.join(", ")));
        };
        let mem = &p.aux.mems[mi];
        for (addr, v) in read_hex(&crate::read(file)?)? {
            if (0..mem.depth as i64).contains(&addr) {
                for x in [mi as u64, addr as u64, v & synth_sim::program::mask(mem.width)] {
                    load.extend_from_slice(&x.to_ne_bytes());
                }
            }
        }
    }
    a.finish()?;

    let model = emit_c::emit_observing(p, PREFIX, &observed)?;
    let main = driver(clk, &sets, &at, until, cycles, &prints, uart, trace, !load.is_empty());
    let dir = TempDir(std::env::temp_dir().join(format!(
        "synth-sim-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    )));
    std::fs::create_dir_all(&dir.0).map_err(|e| format!("{}: {e}", dir.0.display()))?;
    let file = |n: &str| dir.0.join(n);
    let write = |n: &str, data: &[u8]| {
        std::fs::write(file(n), data).map_err(|e| format!("{}: {e}", file(n).display()))
    };
    write(&format!("{PREFIX}.h"), model.header.as_bytes())?;
    write(&format!("{PREFIX}.c"), model.source.as_bytes())?;
    write("main.c", main.as_bytes())?;
    write("load.bin", &load)?;

    // Build.
    let cc = std::env::var("CC").ok().filter(|s| !s.trim().is_empty()).unwrap_or("cc".into());
    let cflags = std::env::var("CFLAGS").ok().filter(|s| !s.trim().is_empty());
    let cflags = cflags.as_deref().unwrap_or("-O1");
    let exe = file(if cfg!(windows) { "sim.exe" } else { "sim" });
    let mut cmd = Command::new(&cc);
    cmd.args(cflags.split_whitespace()).arg("-o").arg(&exe).arg(file("main.c"));
    cmd.arg(file(&format!("{PREFIX}.c")));
    let built = cmd
        .output()
        .map_err(|e| format!("--compiled: cannot run the C compiler `{cc}` ({e}); set CC"))?;
    if !built.status.success() {
        let err = String::from_utf8_lossy(&built.stderr);
        let tail: Vec<&str> = err.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(format!("--compiled: `{cc} {cflags}` failed:\n{}", tail.join("\n")));
    }
    if !quiet {
        eprintln!(
            "info: compiled in {:.2?} ({cc} {cflags}): {} instructions, {} state words, 2-state",
            t0.elapsed(),
            p.ops.len(),
            p.init_val.len()
        );
    }

    // Run: the output passes through, the status comes last on stderr.
    let mut child = Command::new(&exe)
        .arg(file("load.bin"))
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).map_err(|e| e.to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    let mut result = None;
    for line in err.lines() {
        match line.strip_prefix("synth-sim-status ") {
            Some(r) => result = Some(r.to_string()),
            None => eprintln!("{line}"),
        }
    }
    let r: Vec<u64> = match (&result, status.success()) {
        (Some(r), true) => r.split_whitespace().filter_map(|x| x.parse().ok()).collect(),
        _ => return Err(format!("the compiled simulation failed ({status})")),
    };
    let (cycle, ns, stopped) = (r[0], r[1], r[2] != 0);
    if until.is_some() && !stopped {
        return Err(format!("the --until condition did not hold within {cycles} cycles"));
    }
    if !quiet {
        let el = Duration::from_nanos(ns);
        eprintln!(
            "info: {cycle} cycles in {el:.2?} ({:.0} cycles/s){}",
            cycle as f64 / el.as_secs_f64().max(1e-9),
            if stopped { ", stopped by --until" } else { "" }
        );
    }
    if stats {
        let mut kinds = std::collections::BTreeMap::<String, usize>::new();
        for o in &p.ops {
            *kinds.entry(format!("{:?}", o.k)).or_default() += 1;
        }
        let seq: usize = p
            .domains
            .iter()
            .map(|d| d.sample.len() + d.write.len() + d.commit.len() + d.gated.len())
            .sum();
        eprintln!("info: instructions: {kinds:?}; {seq} in clock domains");
    }
    Ok(())
}

/// The C driver of a run.
#[allow(clippy::too_many_arguments)]
fn driver(
    clk: Option<usize>,
    sets: &[(usize, u64)],
    at: &[(u64, usize, u64)],
    until: Option<(usize, Option<u64>)>,
    cycles: u64,
    prints: &[(String, usize)],
    uart: Option<(usize, u64)>,
    trace: bool,
    load: bool,
) -> String {
    let pre = PREFIX;
    let mut c = String::new();
    writeln!(c, "/* Generated by synth sim --compiled. */").unwrap();
    writeln!(
        c,
        "#include <stdio.h>\n#include <stdint.h>\n#include <time.h>\n#include \"{pre}.h\"\n"
    )
    .unwrap();
    writeln!(c, "static {pre}_t m;\n").unwrap();
    writeln!(c, "static void show(uint64_t x) {{ printf(\"%llu (0x%llx)\", (unsigned long long)x, (unsigned long long)x); }}\n").unwrap();
    if let Some((_, cpb)) = uart {
        // As `Uart` in sim.rs: 8N1, the line sampled once per cycle.
        writeln!(
            c,
            "static uint64_t u_prev = 1, u_start, u_byte; static int u_on;
static void uart(uint64_t v, uint64_t cycle) {{
    uint64_t prev = u_prev, cpb = {cpb}ull, mid = cpb / 2, t, k;
    u_prev = v;
    if (!u_on) {{
        if (prev == 1 && v == 0) {{ u_on = 1; u_start = cycle; u_byte = 0; }}
        return;
    }}
    t = cycle - u_start;
    if (t > mid && (t - mid) % cpb == 0) {{
        k = (t - mid) / cpb;
        if (k >= 1 && k <= 8) u_byte |= v << (k - 1);
        else if (k == 9) {{ u_on = 0; putchar((int)u_byte); fflush(stdout); }}
    }}
}}
"
        )
        .unwrap();
    }
    writeln!(c, "int main(int argc, char **argv) {{").unwrap();
    writeln!(
        c,
        "    unsigned long long cycle = 0, cycles = {cycles}ull; int stopped = 0; unsigned next = 0;"
    )
    .unwrap();
    writeln!(c, "    struct timespec t0, t1; long long ns;").unwrap();
    writeln!(c, "    (void)argc; (void)argv; (void)next;").unwrap();
    writeln!(c, "    {pre}_init(&m);").unwrap();
    if load {
        writeln!(
            c,
            "    {{ FILE *f = fopen(argv[1], \"rb\"); uint64_t r[3];
      if (!f) {{ perror(argv[1]); return 1; }}
      while (fread(r, sizeof r[0], 3, f) == 3) {pre}_load_memory(&m, (unsigned)r[0], r[1], r[2]);
      fclose(f); {pre}_memories_written(&m); }}"
        )
        .unwrap();
    }
    for &(si, v) in sets {
        writeln!(c, "    {}", poke(si, v)).unwrap();
    }
    match clk {
        Some(k) => writeln!(c, "    {} {pre}_eval_quiet(&m);", poke(k, 0)).unwrap(),
        None => writeln!(c, "    {pre}_eval(&m);").unwrap(),
    }
    writeln!(c, "    timespec_get(&t0, TIME_UTC);").unwrap();
    writeln!(c, "    while (cycle < cycles) {{").unwrap();
    if !at.is_empty() {
        let cs: Vec<String> = at.iter().map(|x| format!("{}ull", x.0)).collect();
        writeln!(c, "        static const unsigned long long at[] = {{ {} }};", cs.join(", "))
            .unwrap();
        writeln!(c, "        while (next < {} && at[next] <= cycle) {{", at.len()).unwrap();
        writeln!(c, "            switch (next) {{").unwrap();
        for (i, &(_, si, v)) in at.iter().enumerate() {
            writeln!(c, "            case {i}: {} break;", poke(si, v)).unwrap();
        }
        writeln!(c, "            }}\n            next++;\n        }}").unwrap();
    }
    writeln!(c, "        {pre}_eval(&m);").unwrap();
    if let Some((si, want)) = until {
        let cond = match want {
            Some(w) => format!("{} == {w}ull", peek(si)),
            None => format!("{} != 0", peek(si)),
        };
        writeln!(c, "        if ({cond}) {{ stopped = 1; break; }}").unwrap();
    }
    match clk {
        None => writeln!(c, "        cycle++;").unwrap(),
        Some(k) => {
            writeln!(c, "        {} {pre}_eval(&m);", poke(k, 1)).unwrap();
            writeln!(c, "        {} {pre}_eval(&m);", poke(k, 0)).unwrap();
            writeln!(c, "        cycle++;").unwrap();
            if let Some((si, _)) = uart {
                writeln!(c, "        uart({} & 1, cycle);", peek(si)).unwrap();
            }
            if trace {
                writeln!(c, "        printf(\"%llu: \", cycle);").unwrap();
                for (i, (n, si)) in prints.iter().enumerate() {
                    let sep = if i == 0 { "" } else { " " };
                    writeln!(
                        c,
                        "        printf({}); show({});",
                        c_str(&format!("{sep}{n}=")),
                        peek(*si)
                    )
                    .unwrap();
                }
                writeln!(c, "        printf(\"\\n\");").unwrap();
            }
        }
    }
    writeln!(c, "    }}").unwrap();
    writeln!(c, "    timespec_get(&t1, TIME_UTC);").unwrap();
    if !trace {
        for (n, si) in prints {
            writeln!(
                c,
                "    printf({}); show({}); printf(\"\\n\");",
                c_str(&format!("{n} = ")),
                peek(*si)
            )
            .unwrap();
        }
    }
    writeln!(c, "    fflush(stdout);").unwrap();
    writeln!(
        c,
        "    ns = (long long)(t1.tv_sec - t0.tv_sec) * 1000000000ll + (t1.tv_nsec - t0.tv_nsec);"
    )
    .unwrap();
    writeln!(
        c,
        "    fprintf(stderr, \"synth-sim-status %llu %lld %d\\n\", cycle, ns < 0 ? 0 : ns, stopped);"
    )
    .unwrap();
    writeln!(c, "    return 0;\n}}").unwrap();
    c
}
