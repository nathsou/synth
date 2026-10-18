//! The `synth` command-line tool.

mod args;

use std::process::ExitCode;

use args::Args;

const USAGE: &str = "\
synth — unified FPGA toolchain

USAGE:
    synth <COMMAND> [OPTIONS]

COMMANDS:
    check      Check Verilog or Weft sources and report errors
    build      Synthesise, place and route a design into an iCE40 bitstream
    synth      Synthesise a design to iCE40 primitives (--verilog <out.v>)
    elab       Elaborate a design into RTL (--verilog <out.v> to write it back)
    pnr        Place and route a mapped netlist (RTLIL) for iCE40
    pack-asc   Convert an IceStorm .asc file to a binary bitstream
    unpack     Convert a binary bitstream to an IceStorm .asc file
    help       Print this message
    version    Print version information

INPUTS:
    Verilog-2005 (`.v`) or Weft (`.weft`, see docs/WEFT.md). Verilog files
    given with Weft sources define the Weft design's extern modules.

BUILD OPTIONS:
    synth build <files.v|.weft...> --device <hx1k|hx8k|...> --package <pkg>
              [--top <module>] [-I <dir>] [-D <NAME[=VAL]>] [-P <NAME=VALUE>]
              [--pcf <file>] [-o <out.bin>] [--asc <out.asc>] [--seed <n>]
              [--timing-weight <0..1>] [--freq <MHz>] [--timing-allow-fail]
              [--carry-min-width <bits>]
              [--route-timing-rounds <n>] [--incremental [<checkpoint>]]
              --carry-min-width: force a minimum adder width for hard carry
              chains; variable comparators use two more bits. By default,
              compare thresholds 3 and 8 and keep a shallower candidate only
              when it uses no more packed logic cells.
              Clock targets: `set_frequency <net> <MHz>` in the PCF, --freq
              for the other clocks. Missing a target fails the build (the
              bitstream is still written) unless --timing-allow-fail.
              --route-timing-rounds: reroute critical connections that many
              times once routing is legal, keeping the best timed routing
              (slower; helps congested designs slightly).
              --incremental: reuse compatible placement/routes and update
              the checkpoint after a run that meets timing. Missing, corrupt
              or incompatible checkpoints start fresh. Synthesis still runs.
              Without a path, select .synth/checkpoints/<design-key>.chk.
              Put bare --incremental after input files or before another option.

ELAB OPTIONS:
    synth elab <files.v...> [--top <module>] [-I <dir>] [-D <NAME[=VAL]>]
              [-P <NAME=VALUE>] [--verilog <out.v>] [--rename <module>]
              [--pass <clean|merge|share|trees|fsm|wreduce|shiftmul|dff|memff|memtrans|memregs|two-state>]...

PNR OPTIONS:
    synth pnr <netlist.il> --device <hx1k|hx8k|...> --package <pkg>
              [--pcf <file>] [-o <out.bin>] [--asc <out.asc>] [--seed <n>]
              [--freq <MHz>] [--timing-allow-fail] [--route-timing-rounds <n>]
              [--incremental [<checkpoint>]]
";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let result = match argv.first().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("version" | "-V" | "--version") => {
            println!("synth {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("check") => cmd_check(&argv[1..]),
        Some("elab") => cmd_elab(&argv[1..]),
        Some("build") => cmd_build(&argv[1..]),
        Some("synth") => cmd_synth(&argv[1..]),
        Some("pnr") => cmd_pnr(&argv[1..]),
        Some("pack-asc") => cmd_pack_asc(&argv[1..]),
        Some("unpack") => cmd_unpack(&argv[1..]),
        Some(other) => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

fn write(path: &str, data: &[u8]) -> Result<(), String> {
    std::fs::write(path, data).map_err(|e| format!("{path}: {e}"))
}

struct Ice40Lib;

impl synth_rtlil::PrimitiveLibrary for Ice40Lib {
    fn port_dir(&self, kind: &str, port: &str) -> Option<synth_netlist::Dir> {
        arch_ice40::cells::port_dir(kind, port)
    }
}

/// Elaborates and synthesises the design named by `a` for iCE40.
fn synthesize(a: &mut Args) -> Result<synth_netlist::Netlist, String> {
    let t = std::time::Instant::now();
    let m = load_rtl(a)?;
    eprintln!("info: elaborated in {:.2}s", t.elapsed().as_secs_f64());
    let mut synth_opts = arch_ice40::synth::SynthOptions::default();
    if let Some(value) = a.value("--carry-min-width") {
        let width = value
            .parse::<usize>()
            .ok()
            .filter(|&w| w > 0 && w.checked_add(2).is_some())
            .ok_or("--carry-min-width must be a positive integer with room for the comparator offset (+2)")?;
        synth_opts.lower.carry_min_width = Some(width);
        synth_opts.explore_carry = false;
    }
    let (nl, log) = arch_ice40::synth::synthesize(m, &synth_opts)?;
    for l in &log {
        eprintln!("info: {l}");
    }
    let stats = nl.stats();
    eprintln!(
        "info: {}",
        stats.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", ")
    );
    Ok(nl)
}

/// `--timing-weight` (default 0.5).
/// Clock targets: the PCF's `set_frequency` lines, and `--freq` for the
/// other clocks.
fn clocks(
    a: &mut Args,
    pcf: &arch_ice40::pcf::Pcf,
) -> Result<arch_ice40::timing::ClockConstraints, String> {
    let mut c = pcf.clocks.clone();
    c.default_mhz = a.value("--freq").map(|s| s.parse().map_err(|_| "bad --freq")).transpose()?;
    Ok(c)
}

fn timing_weight(a: &mut Args) -> Result<f64, String> {
    match a.value("--timing-weight") {
        Some(v) => v
            .parse()
            .ok()
            .filter(|w: &f64| (0.0..=1.0).contains(w))
            .ok_or("bad --timing-weight".into()),
        None => Ok(0.5),
    }
}

fn cmd_synth(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(
        argv,
        &["-I", "-D", "-P", "--top", "--verilog", "--rename", "--carry-min-width"],
        &[],
    )?;
    let nl = synthesize(&mut a)?;
    if let Some(out) = a.value("--verilog") {
        let name = a.value("--rename").unwrap_or_else(|| {
            nl.top.map(|t| nl.str(t).to_string()).unwrap_or_else(|| "top".into())
        });
        write(&out, synth_netlist::verilog::write(&nl, &name).as_bytes())?;
    }
    Ok(())
}

fn cmd_build(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse_optional(
        argv,
        &[
            "-I",
            "-D",
            "-P",
            "--top",
            "--device",
            "--package",
            "--pcf",
            "-o",
            "--asc",
            "--seed",
            "--effort",
            "--carry-min-width",
            "--timing-weight",
            "--freq",
            "--route-timing-rounds",
            "--incremental",
        ],
        &["--timing-allow-fail"],
        &["--incremental"],
    )?;
    let part: arch_ice40::arch::Part = a.required("--device")?.parse()?;
    let package = a.required("--package")?;
    let pcf = match a.value("--pcf") {
        Some(p) => arch_ice40::pcf::parse_pcf(&read(&p)?)?,
        None => Default::default(),
    };
    let clocks = clocks(&mut a, &pcf)?;
    let seed =
        a.value("--seed").map(|s| s.parse().map_err(|_| "bad --seed")).transpose()?.unwrap_or(1);
    let effort = a
        .value("--effort")
        .map(|s| s.parse().map_err(|_| "bad --effort"))
        .transpose()?
        .unwrap_or(1.0);
    let nl = synthesize(&mut a)?;
    let opts = arch_ice40::flow::FlowOptions {
        part,
        package,
        seed,
        place_effort: effort,
        timing_weight: timing_weight(&mut a)?,
        clocks,
        route_timing_rounds: a
            .value("--route-timing-rounds")
            .map(|s| s.parse().map_err(|_| "bad --route-timing-rounds"))
            .transpose()?
            .unwrap_or(0),
    };
    let res = implement(nl, &pcf.pins, &opts, &mut a)?;
    finish_flow(&res, &mut a)
}

fn cmd_pnr(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse_optional(
        argv,
        &[
            "--device",
            "--package",
            "--pcf",
            "-o",
            "--asc",
            "--seed",
            "--effort",
            "--timing-weight",
            "--freq",
            "--route-timing-rounds",
            "--incremental",
        ],
        &["--timing-allow-fail"],
        &["--incremental"],
    )?;
    let input = a.positional(0, "netlist")?;
    let part: arch_ice40::arch::Part = a.required("--device")?.parse()?;
    let package = a.required("--package")?;
    let pcf = match a.value("--pcf") {
        Some(p) => arch_ice40::pcf::parse_pcf(&read(&p)?)?,
        None => Default::default(),
    };
    let clocks = clocks(&mut a, &pcf)?;
    let seed =
        a.value("--seed").map(|s| s.parse().map_err(|_| "bad --seed")).transpose()?.unwrap_or(1);
    let effort = a
        .value("--effort")
        .map(|s| s.parse().map_err(|_| "bad --effort"))
        .transpose()?
        .unwrap_or(1.0);
    a.finish()?;
    let design = synth_rtlil::parse(&read(&input)?).map_err(|e| e.to_string())?;
    let nl = synth_rtlil::to_netlist(&design, &Ice40Lib)?;
    let opts = arch_ice40::flow::FlowOptions {
        part,
        package,
        seed,
        place_effort: effort,
        timing_weight: timing_weight(&mut a)?,
        clocks,
        route_timing_rounds: a
            .value("--route-timing-rounds")
            .map(|s| s.parse().map_err(|_| "bad --route-timing-rounds"))
            .transpose()?
            .unwrap_or(0),
    };
    let res = implement(nl, &pcf.pins, &opts, &mut a)?;
    finish_flow(&res, &mut a)
}

/// Selects an optional implementation checkpoint and runs PnR.
fn implement(
    nl: synth_netlist::Netlist,
    pins: &arch_ice40::place::PinConstraints,
    opts: &arch_ice40::flow::FlowOptions,
    a: &mut Args,
) -> Result<arch_ice40::flow::FlowResult, String> {
    let Some(path) = checkpoint_path(a, opts)? else {
        return arch_ice40::flow::implement(nl, pins, opts);
    };
    let previous = match std::fs::read(&path) {
        Ok(bytes) => match arch_ice40::checkpoint::Checkpoint::decode(&bytes) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("info: ignoring checkpoint {path}: {e}; using fresh PnR");
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{path}: {e}")),
    };
    arch_ice40::flow::implement_incremental(nl, pins, opts, previous.as_ref())
}

fn checkpoint_path(
    a: &mut Args,
    opts: &arch_ice40::flow::FlowOptions,
) -> Result<Option<String>, String> {
    use std::hash::Hasher;
    let Some(path) = a.value("--incremental") else { return Ok(None) };
    if !path.is_empty() {
        return Ok(Some(path));
    }
    let mut key = synth_core::FxHasher::default();
    let canonical = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| p.into());
    // Identity, not content: edits must find the previous implementation.
    // Includes/defines/parameters are ordered because their order can matter.
    for i in 0.. {
        let Ok(p) = a.positional(i, "input") else { break };
        key.write(format!("input {:?}\n", canonical(&p)).as_bytes());
    }
    for name in ["--top", "-I", "-D", "-P", "--pcf"] {
        let values: Vec<_> =
            a.values(name)
                .into_iter()
                .map(|v| {
                    if matches!(name, "-I" | "--pcf") { format!("{:?}", canonical(&v)) } else { v }
                })
                .collect();
        key.write(format!("{name} {values:?}\n").as_bytes());
    }
    // Keep fixed policies and automatic exploration in separate checkpoint
    // namespaces, including checkpoints from the old fixed-width default.
    if let Some(width) = a.value("--carry-min-width") {
        key.write(format!("carry-min-width {width}\n").as_bytes());
    } else {
        key.write(b"carry-policy auto-v1\n");
    }
    key.write(format!("{opts:?}").as_bytes());
    let dir = ".synth/checkpoints";
    std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    let path = format!("{dir}/{}-{}-{:016x}.chk", opts.part, opts.package, key.finish());
    a.set_value("--incremental", path.clone());
    eprintln!("info: incremental checkpoint: {path}");
    Ok(Some(path))
}

// Write beside the destination and rename only after all bytes are complete.
// A failed build/write leaves the previous checkpoint available for reuse.
fn write_checkpoint(path: &str, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let temporary = format!("{path}.{}.tmp", std::process::id());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| format!("{temporary}: {e}"))?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|e| format!("{path}: {e}"))
}

/// Writes outputs. A missed target is an error unless explicitly allowed,
/// and never replaces the previous checkpoint.
fn finish_flow(res: &arch_ice40::flow::FlowResult, a: &mut Args) -> Result<(), String> {
    for l in &res.log {
        eprintln!("info: {l}");
    }
    if let Some(out) = a.value("-o") {
        write(&out, &res.config.to_bin())?;
    }
    if let Some(out) = a.value("--asc") {
        write(&out, arch_ice40::asc::write_asc(&res.config).as_bytes())?;
    }
    if res.timing_met
        && let Some(checkpoint) = &res.checkpoint
        && let Some(path) = a.value("--incremental")
    {
        write_checkpoint(&path, &checkpoint.encode())?;
    }
    if !res.timing_met && !a.flag("--timing-allow-fail") {
        return Err("timing targets not met (--timing-allow-fail accepts this)".into());
    }
    Ok(())
}

fn cmd_pack_asc(argv: &[String]) -> Result<(), String> {
    let a = Args::parse(argv, &[], &[])?;
    let input = a.positional(0, "input .asc")?;
    let output = a.positional(1, "output .bin")?;
    let cfg = arch_ice40::asc::read_asc(&read(&input)?).map_err(|e| format!("{input}: {e}"))?;
    write(&output, &cfg.to_bin())
}

fn cmd_unpack(argv: &[String]) -> Result<(), String> {
    let a = Args::parse(argv, &[], &[])?;
    let input = a.positional(0, "input .bin")?;
    let output = a.positional(1, "output .asc")?;
    let data = std::fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    let cfg = arch_ice40::Config::from_bin(&data).map_err(|e| e.to_string())?;
    write(&output, arch_ice40::asc::write_asc(&cfg).as_bytes())
}

/// The input files given on the command line.
fn input_files(a: &Args) -> Vec<String> {
    (0..).map_while(|i| a.positional(i, "file").ok()).collect()
}

fn is_weft(path: &str) -> bool {
    path.ends_with(".weft")
}

/// Elaborates the design given on the command line, Verilog or Weft, into
/// RTL. Weft sources may be mixed with Verilog files that define their
/// extern modules.
fn load_rtl(a: &mut Args) -> Result<synth_rtl::Module, String> {
    let paths = input_files(a);
    if !paths.iter().any(|p| is_weft(p)) {
        let top_params = a.values("-P").iter().map(|p| parse_param(p)).collect::<Result<_, _>>()?;
        let (sm, src) = load_verilog(a)?;
        let opts = synth_elab::ElabOptions { top: a.value("--top"), top_params };
        let (m, diags) = synth_elab::elaborate(&src, &sm, &opts, &synth_elab::NoBlackBoxes);
        if !diags.items.is_empty() {
            eprint!("{}", diags.render(&sm));
        }
        return m.ok_or_else(|| format!("{} error(s)", diags.error_count()));
    }
    let mut params = Vec::new();
    for p in a.values("-P") {
        let (k, v) = p.split_once('=').ok_or_else(|| format!("expected NAME=VALUE, got `{p}`"))?;
        params.push((k.to_string(), v.to_string()));
    }
    let mut sm = synth_core::SourceMap::new();
    let mut weft = Vec::new();
    let mut verilog = Vec::new();
    for p in &paths {
        let id = sm.add(p.clone(), read(p)?);
        if is_weft(p) { weft.push(id) } else { verilog.push(id) }
    }
    let src = if verilog.is_empty() {
        None
    } else {
        let (src, diags) = synth_verilog::parse_files(&mut sm, &verilog, &preproc_options(a));
        if !diags.items.is_empty() {
            eprint!("{}", diags.render(&sm));
        }
        if diags.has_errors() {
            return Err(format!("{} error(s)", diags.error_count()));
        }
        Some(src)
    };
    let opts = synth_weft::Options { top: a.value("--top"), params };
    let prelude = [("ice40 primitives", arch_ice40::WEFT_PRELUDE)];
    let (m, diags) = synth_weft::compile(&mut sm, &weft, &prelude, src.as_ref(), &opts);
    if !diags.items.is_empty() {
        eprint!("{}", diags.render(&sm));
    }
    let warnings =
        diags.items.iter().filter(|d| d.severity == synth_core::Severity::Warning).count();
    match m {
        Some(m) => {
            if warnings > 0 {
                eprintln!("info: {warnings} warning(s)");
            }
            Ok(m)
        }
        None => Err(format!("{} error(s), {warnings} warning(s)", diags.error_count())),
    }
}

fn preproc_options(a: &mut Args) -> synth_verilog::PreprocOptions {
    let mut opts = synth_verilog::PreprocOptions::default();
    for i in a.values("-I") {
        opts.include_dirs.push(i.into());
    }
    for d in a.values("-D") {
        let (k, v) = d.split_once('=').unwrap_or((&d, "1"));
        opts.defines.push((k.to_string(), v.to_string()));
    }
    opts
}

/// Parses Verilog sources given on the command line (`-I dir`, `-D name=value`).
fn load_verilog(
    a: &mut Args,
) -> Result<(synth_core::SourceMap, synth_verilog::ast::SourceText), String> {
    let mut sm = synth_core::SourceMap::new();
    let mut opts = synth_verilog::PreprocOptions::default();
    for i in a.values("-I") {
        opts.include_dirs.push(i.into());
    }
    for d in a.values("-D") {
        let (k, v) = d.split_once('=').unwrap_or((&d, "1"));
        opts.defines.push((k.to_string(), v.to_string()));
    }
    let mut files = Vec::new();
    for i in 0.. {
        let Ok(path) = a.positional(i, "file") else { break };
        files.push(sm.add(path.clone(), read(&path)?));
    }
    if files.is_empty() {
        return Err("no input files".into());
    }
    let (src, diags) = synth_verilog::parse_files(&mut sm, &files, &opts);
    if !diags.items.is_empty() {
        eprint!("{}", diags.render(&sm));
    }
    if diags.has_errors() {
        return Err(format!("{} error(s)", diags.error_count()));
    }
    Ok((sm, src))
}

/// Runs a named RTL pass (`synth elab --pass`), for testing passes by
/// differential simulation.
fn run_rtl_pass(m: &mut synth_rtl::Module, name: &str) -> Result<(), String> {
    use synth_rtl::{MemId, mem, opt};
    let mems = || (0..m.memories.len()).map(MemId::new).collect::<Vec<_>>();
    match name {
        "clean" => opt::clean(m)?,
        "two-state" => opt::two_state(m),
        "merge" => {
            let n = opt::merge_identical(m);
            eprintln!("info: merge: {n} duplicate cells");
        }
        "fsm" => {
            let n = synth_rtl::fsm::simplify_state_compares(m);
            eprintln!("info: fsm: {n} state comparisons simplified");
        }
        "trees" => {
            let n = opt::chains_to_trees(m);
            eprintln!("info: trees: {n} multiplexer chains as trees");
        }
        "share" => {
            let n = opt::share_arith(m);
            eprintln!("info: share: {n} multiplexed operations shared");
        }
        "wreduce" => {
            let n = opt::reduce_widths(m);
            eprintln!("info: wreduce: {n} result bits constant");
        }
        "shiftmul" => {
            let n = opt::shift_by_multiple(m);
            eprintln!("info: shiftmul: {n} shifts by multiples as multiplexers");
        }
        "dff" => {
            let n = synth_rtl::dff::extract_controls(m);
            eprintln!("info: dff: {n} flip-flop bits gained controls");
        }
        "memff" => {
            if mem::remove_feedback_writes(m) > 0 {
                opt::clean(m)?;
            }
            mem::merge_write_ports(m);
            let n = mem::merge_read_ffs(m);
            eprintln!("info: memff: {n} read ports made synchronous");
        }
        "memtrans" => {
            for mi in mems() {
                if m.memories[mi].alive {
                    mem::emulate_transparency(m, mi);
                }
            }
        }
        "memregs" => {
            for mi in mems() {
                if m.memories[mi].alive {
                    mem::to_registers(m, mi)?;
                }
            }
        }
        _ => {
            return Err(format!(
                "unknown pass `{name}` (clean, merge, share, trees, fsm, wreduce, shiftmul, dff, memff, memtrans, memregs, two-state)"
            ));
        }
    }
    opt::clean(m)
}

/// Parses a top-level parameter override `NAME=VALUE`, where the value is a
/// decimal or `0x` hexadecimal integer or a double-quoted string.
fn parse_param(p: &str) -> Result<(String, synth_core::LogicVec), String> {
    let (name, v) = p.split_once('=').ok_or_else(|| format!("expected NAME=VALUE, got `{p}`"))?;
    let bad = || format!("bad parameter value `{v}` (expected an integer or \"string\")");
    let value = if let Some(s) = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        let bits: Vec<synth_core::Logic> = s
            .bytes()
            .rev()
            .flat_map(|b| (0..8).map(move |i| synth_core::Logic::from_bool(b >> i & 1 == 1)))
            .collect();
        synth_core::LogicVec::from_bits(&bits)
    } else if let Some(h) = v.strip_prefix("0x") {
        synth_core::LogicVec::parse_radix(h, 16, 32).ok_or_else(bad)?
    } else {
        let n: i64 = v.parse().map_err(|_| bad())?;
        synth_core::LogicVec::from_i64(n, 32)
    };
    Ok((name.to_string(), value))
}

fn cmd_check(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(argv, &["-I", "-D", "-P", "--top"], &[])?;
    if input_files(&a).iter().any(|p| is_weft(p)) {
        let m = load_rtl(&mut a)?;
        eprintln!("info: `{}` checked: no errors", m.name);
        return Ok(());
    }
    let (_, src) = load_verilog(&mut a)?;
    eprintln!("info: parsed {} module(s)", src.modules.len());
    Ok(())
}

fn cmd_elab(argv: &[String]) -> Result<(), String> {
    let mut a =
        Args::parse(argv, &["-I", "-D", "-P", "--top", "--verilog", "--rename", "--pass"], &[])?;
    let passes = a.values("--pass");
    let mut m = load_rtl(&mut a)?;
    for p in &passes {
        run_rtl_pass(&mut m, p)?;
    }
    let stats = m.stats();
    eprintln!(
        "info: {} wires, cells: {}",
        m.wires.len(),
        stats.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", ")
    );
    if let Some(r) = a.value("--rename") {
        m.name = r;
    }
    if let Some(out) = a.value("--verilog") {
        write(&out, synth_rtl::verilog::write(&m).as_bytes())?;
    }
    Ok(())
}
