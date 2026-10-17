//! The `synth` command-line tool.

mod args;

use std::process::ExitCode;

use args::Args;

const USAGE: &str = "\
synth — unified FPGA toolchain

USAGE:
    synth <COMMAND> [OPTIONS]

COMMANDS:
    check      Parse Verilog files and report errors
    build      Synthesise, place and route Verilog into an iCE40 bitstream
    synth      Synthesise Verilog to iCE40 primitives (--verilog <out.v>)
    elab       Elaborate Verilog into RTL (--verilog <out.v> to write it back)
    pnr        Place and route a mapped netlist (RTLIL) for iCE40
    pack-asc   Convert an IceStorm .asc file to a binary bitstream
    unpack     Convert a binary bitstream to an IceStorm .asc file
    help       Print this message
    version    Print version information

BUILD OPTIONS:
    synth build <files.v...> --device <hx1k|hx8k|...> --package <pkg>
              [--top <module>] [-I <dir>] [-D <NAME[=VAL]>] [-P <NAME=VALUE>]
              [--pcf <file>] [-o <out.bin>] [--asc <out.asc>] [--seed <n>]
              [--timing-weight <0..1>]

ELAB OPTIONS:
    synth elab <files.v...> [--top <module>] [-I <dir>] [-D <NAME[=VAL]>]
              [-P <NAME=VALUE>] [--verilog <out.v>] [--rename <module>]
              [--pass <clean|merge|share|trees|fsm|dff|memff|memtrans|memregs|two-state>]...

PNR OPTIONS:
    synth pnr <netlist.il> --device <hx1k|hx8k|...> --package <pkg>
              [--pcf <file>] [-o <out.bin>] [--asc <out.asc>] [--seed <n>]
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

/// Elaborates and synthesises the Verilog named by `a` for iCE40.
fn synthesize(a: &mut Args) -> Result<synth_netlist::Netlist, String> {
    let top_params = a.values("-P").iter().map(|p| parse_param(p)).collect::<Result<_, _>>()?;
    let t = std::time::Instant::now();
    let (sm, src) = load_verilog(a)?;
    let opts = synth_elab::ElabOptions { top: a.value("--top"), top_params };
    let (m, diags) = synth_elab::elaborate(&src, &sm, &opts, &synth_elab::NoBlackBoxes);
    if !diags.items.is_empty() {
        eprint!("{}", diags.render(&sm));
    }
    let m = m.ok_or_else(|| format!("{} error(s)", diags.error_count()))?;
    eprintln!("info: elaborated in {:.2}s", t.elapsed().as_secs_f64());
    let (nl, log) = arch_ice40::synth::synthesize(m, &arch_ice40::synth::SynthOptions::default())?;
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
    let mut a = Args::parse(argv, &["-I", "-D", "-P", "--top", "--verilog", "--rename"], &[])?;
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
    let mut a = Args::parse(
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
            "--timing-weight",
        ],
        &[],
    )?;
    let part: arch_ice40::arch::Part = a.required("--device")?.parse()?;
    let package = a.required("--package")?;
    let pins = match a.value("--pcf") {
        Some(p) => arch_ice40::pcf::parse_pcf(&read(&p)?)?,
        None => Default::default(),
    };
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
    };
    let res = arch_ice40::flow::implement(nl, &pins, &opts)?;
    for l in &res.log {
        eprintln!("info: {l}");
    }
    if let Some(out) = a.value("-o") {
        write(&out, &res.config.to_bin())?;
    }
    if let Some(out) = a.value("--asc") {
        write(&out, arch_ice40::asc::write_asc(&res.config).as_bytes())?;
    }
    Ok(())
}

fn cmd_pnr(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(
        argv,
        &["--device", "--package", "--pcf", "-o", "--asc", "--seed", "--effort", "--timing-weight"],
        &[],
    )?;
    let input = a.positional(0, "netlist")?;
    let part: arch_ice40::arch::Part = a.required("--device")?.parse()?;
    let package = a.required("--package")?;
    let pins = match a.value("--pcf") {
        Some(p) => arch_ice40::pcf::parse_pcf(&read(&p)?)?,
        None => Default::default(),
    };
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
    };
    let res = arch_ice40::flow::implement(nl, &pins, &opts)?;
    for l in &res.log {
        eprintln!("info: {l}");
    }
    if let Some(out) = a.value("-o") {
        write(&out, &res.config.to_bin())?;
    }
    if let Some(out) = a.value("--asc") {
        write(&out, arch_ice40::asc::write_asc(&res.config).as_bytes())?;
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
                "unknown pass `{name}` (clean, merge, share, trees, fsm, dff, memff, memtrans, memregs, two-state)"
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
    let mut a = Args::parse(argv, &["-I", "-D"], &[])?;
    let (_, src) = load_verilog(&mut a)?;
    eprintln!("info: parsed {} module(s)", src.modules.len());
    Ok(())
}

fn cmd_elab(argv: &[String]) -> Result<(), String> {
    let mut a =
        Args::parse(argv, &["-I", "-D", "-P", "--top", "--verilog", "--rename", "--pass"], &[])?;
    let passes = a.values("--pass");
    let top_params = a.values("-P").iter().map(|p| parse_param(p)).collect::<Result<_, _>>()?;
    let (sm, src) = load_verilog(&mut a)?;
    let opts = synth_elab::ElabOptions { top: a.value("--top"), top_params };
    let (m, diags) = synth_elab::elaborate(&src, &sm, &opts, &synth_elab::NoBlackBoxes);
    if !diags.items.is_empty() {
        eprint!("{}", diags.render(&sm));
    }
    let mut m = m.ok_or_else(|| format!("{} error(s)", diags.error_count()))?;
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
