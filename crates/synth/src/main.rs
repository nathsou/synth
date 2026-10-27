//! The `synth` command-line tool.

/// Prints `info: …` to stderr, above any progress line.
macro_rules! info {
    ($($t:tt)*) => {
        synth_core::progress::info(&format!($($t)*))
    };
}

mod args;
mod ecp5;
mod progress;
mod sim;
mod xc7;

use std::process::ExitCode;

use args::Args;

const USAGE: &str = "\
synth — unified FPGA toolchain

USAGE:
    synth <COMMAND> [OPTIONS]

COMMANDS:
    check      Check Verilog or Weft sources and report errors
    build      Synthesise, place and route a design into a bitstream
    synth      Synthesise a design to iCE40 or Artix-7 primitives (--verilog <out.v>)
    elab       Elaborate a design into RTL (--verilog <out.v> to write it back)
    sim        Simulate a design (or write it as a C model with --emit-c)
    pnr        Place and route a mapped netlist (RTLIL)
    pack-fasm  Assemble Artix-7 FASM into a bitstream
    unpack-xc7 Decode Artix-7 frames, checking ECC and CRC
    pack-ecp5  Assemble an ECP5 textual configuration into a bitstream
    unpack-ecp5 Decode an ECP5 bitstream to its textual configuration
    pack-asc   Convert an IceStorm .asc file to a binary bitstream
    unpack     Convert a binary bitstream to an IceStorm .asc file
    fmt        Format Weft sources in place (--check: only report)
    lsp        Run the Weft language server on stdin/stdout
    playground Serve the playground web app (--port <n>, default 8080;
               --xc7-db <prjxray-db/artix7> for the Artix-7 boards)
    help       Print this message
    version    Print version information

INPUTS:
    Verilog-2005 (`.v`) or Weft (`.weft`, see docs/WEFT.md). Verilog files
    given with Weft sources define the Weft design's extern modules.

BUILD OPTIONS:
    synth build <files.v|.weft...> --device <hx1k|hx8k|...> --package <pkg>
              [--top <module>] [-I <dir>] [-D <NAME[=VAL]>] [-P <NAME=VALUE>]
              [--pcf <file>] [-o <out.bin>] [--asc <out.asc>] [--seed <n>]
              [--tries <n>] [--timing-weight <0..1>] [--freq <MHz>]
              [--timing-allow-fail] [--carry-min-width <bits>]
              [--route-timing-rounds <n>] [--incremental [<checkpoint>]]
              --tries: placement seeds placed and routed, on threads of their
              own, the best timed kept (default 4; 1 is fastest).
              --carry-min-width: minimum adder width for hard carry chains
              (default 3); variable comparators use two more bits.
              Clock targets: `set_frequency <net> <MHz>` in the PCF, --freq
              for the other clocks. Missing a target fails the build (the
              bitstream is still written) unless --timing-allow-fail.
              --route-timing-rounds: reroute critical connections that many
              times once routing is legal, keeping the best timed routing
              (default 3; 0 keeps the first legal routing).
              --incremental: reuse compatible placement/routes and update
              the checkpoint after a run that meets timing. Missing, corrupt
              or incompatible checkpoints start fresh. Synthesis still runs.
              Without a path, select .synth/checkpoints/<design-key>.chk.
              Put bare --incremental after input files or before another option.

SYNTH OPTIONS:
    synth synth <files.v|.weft...> [--family <ice40|artix7|xc7>] [--top <module>]
              [-I <dir>] [-D <NAME[=VAL]>] [-P <NAME=VALUE>]
              [--verilog <out.v>] [--rename <module>] [--pack-stats]
              --pack-stats reports native Artix-7 packing resource counts.

ARTIX-7 BUILD/PNR OPTIONS (EXPERIMENTAL):
    synth build <files.v...> --family artix7 --top <module>
              --device <xc7a35tftg256-1|xc7a100tcsg324-1|...>
              --db <prjxray-db/artix7> --xdc <file> [-o <out.bit>]
              [--fasm <out.fasm>] [--seed <n>] [--effort <n>] [--tries <n>]
              --timing-allow-fail
              pnr accepts the same physical options with a mapped RTLIL input;
              pnr --pack-stats reports its native packing (alone without --db).
              Native Rust packing, placement, routing and frame generation.
              Timing closure is not certified; --timing-allow-fail is required.
    synth pack-fasm <input.fasm> --db <db> --device <part> -o <out.bit>
    synth unpack-xc7 <input.bit> --db <db> --device <part> -o <out.frm>

ECP5 OPTIONS:
    synth pack-ecp5 <in.config> -o <out.bit> [--compress] [--db <dir>]
    synth unpack-ecp5 <in.bit> -o <out.config> [--db <dir>]
              Byte for byte as Project Trellis' ecppack and ecpunpack. The
              database (prjtrellis-db) is --db, else SYNTH_TRELLIS_DB, else
              /usr/share/trellis/database (Debian: fpga-trellis-database).

ELAB OPTIONS:
    synth elab <files.v...> [--top <module>] [-I <dir>] [-D <NAME[=VAL]>]
              [-P <NAME=VALUE>] [--verilog <out.v>] [--rename <module>]
              [--pass <clean|merge|share|trees|fsm|wreduce|shiftmul|dff|memff|memtrans|memregs|two-state>]...

SIM OPTIONS:
    synth sim <files.v|.weft...> [--top <module>] [-I <dir>] [-D <NAME[=VAL]>]
              [-P <NAME=VALUE>] [--clock <name>] [--cycles <n>]
              [--until <name>[=<value>]] [--set <name>=<value>]...
              [--at <cycle>:<name>=<value>]... [--print <name>]... [--trace]
              [--vcd <out.vcd>] [--uart <name>[:<clocks per bit>]]
              [--load <memory>=<file.hex>]... [--two-state | --four-state]
              [--attach <name>:<addr>:<data>:<depth>[:<clock>:<enable>:<wdata>]]...
              [--emit-c <dir>] [--level <rtl|synth|bitstream>] [--optimize]
              [--stats] [--quiet]
    synth sim <files...> --compare <synth|bitstream> [--clock <name>]...
              [--reset <name>] [--reset-low <name>] [--cycles <n>] [--hold <n>]
              [--seed <n>]
              (bitstream: --device <dev> --package <pkg> --pcf <file>
              [--bitstream <file.bin>] [--freq <MHz>])
              Runs cycles of the clock (default `clk`): inputs set by --set
              and --at, then a rising and a falling edge. Stops after --cycles
              (default 1000) or once --until holds, then prints --print
              signals (every cycle with --trace). Weft simulates two-state,
              Verilog four-state. --uart decodes an 8N1 output line.
              --attach drives the input <data> with the word at <addr> of a
              new memory (written on the clock's rising edge when <enable>).
              --emit-c writes a two-state C model (<top>.h, <top>.c) instead,
              optimized as --optimize does. --optimize runs synthesis's RTL
              optimizations first and merges bit-level multiplexers and
              flip-flops into words: faster, but internal signals may be
              gone or renamed. --stats prints what the program runs.
              --level synth simulates the iCE40 netlist synthesis makes of
              the design, --level bitstream what its bitstream implements
              (built, or read from --bitstream), decoded back to cells; ports
              are named after the PCF.
              --compare runs the RTL and the netlist or bitstream under the
              same random inputs (held about --hold cycles; resets active for
              the first 4 cycles) and compares every output bit the RTL
              defines after every rising edge.

PNR OPTIONS:
    synth pnr <netlist.il> --device <hx1k|hx8k|...> --package <pkg>
              [--pcf <file>] [-o <out.bin>] [--asc <out.asc>] [--seed <n>]
              [--tries <n>] [--freq <MHz>] [--timing-allow-fail]
              [--route-timing-rounds <n>] [--incremental [<checkpoint>]]
";

/// Whether the command line selects the Artix-7 flow.
fn xc7_family(argv: &[String]) -> bool {
    argv.windows(2).any(|w| w[0] == "--family" && matches!(w[1].as_str(), "artix7" | "xc7"))
        || argv.iter().any(|a| matches!(a.as_str(), "--family=artix7" | "--family=xc7"))
}

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
        Some("sim") => sim::cmd_sim(&argv[1..]),
        // The flows show their progress on a terminal.
        Some("build") => progress::run(xc7_family(&argv), || cmd_build(&argv[1..])),
        Some("synth") => progress::run(xc7_family(&argv), || cmd_synth(&argv[1..])),
        Some("pnr") => progress::run(xc7_family(&argv), || cmd_pnr(&argv[1..])),
        Some("pack-fasm") => xc7::pack_fasm(&argv[1..]),
        Some("unpack-xc7") => xc7::unpack(&argv[1..]),
        Some("pack-ecp5") => ecp5::pack(&argv[1..]),
        Some("unpack-ecp5") => ecp5::unpack(&argv[1..]),
        Some("pack-asc") => cmd_pack_asc(&argv[1..]),
        Some("unpack") => cmd_unpack(&argv[1..]),
        Some("playground") => cmd_playground(&argv[1..]),
        Some("fmt") => cmd_fmt(&argv[1..]),
        Some("lsp") => {
            let prelude =
                vec![("ice40 primitives".to_string(), arch_ice40::WEFT_PRELUDE.to_string())];
            return match synth_lsp::serve(prelude) {
                Ok(0) => ExitCode::SUCCESS,
                Ok(_) => ExitCode::FAILURE,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
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

pub(crate) fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

pub(crate) fn write(path: &str, data: &[u8]) -> Result<(), String> {
    std::fs::write(path, data).map_err(|e| format!("{path}: {e}"))
}

struct Ice40Lib;

impl synth_rtlil::PrimitiveLibrary for Ice40Lib {
    fn port_dir(&self, kind: &str, port: &str) -> Option<synth_netlist::Dir> {
        arch_ice40::cells::port_dir(kind, port)
    }
}

/// Elaborates and synthesises the design for the selected architecture.
fn synthesize(a: &mut Args) -> Result<synth_netlist::Netlist, String> {
    let family = a.value("--family").unwrap_or_else(|| "ice40".into());
    if !matches!(family.as_str(), "ice40" | "artix7" | "xc7" | "ecp5") {
        return Err(format!(
            "unknown synthesis family `{family}` (expected ice40, ecp5, artix7 or xc7)"
        ));
    }
    let t = std::time::Instant::now();
    synth_core::progress::step("elaborate", "");
    let m = load_rtl(a)?;
    info!("elaborated in {:.2}s", t.elapsed().as_secs_f64());
    synth_core::progress::step("synthesize", "");
    let (nl, log) = if family == "ice40" {
        let mut synth_opts = arch_ice40::synth::SynthOptions::default();
        if let Some(value) = a.value("--carry-min-width") {
            let width = value
                .parse::<usize>()
                .ok()
                .filter(|&w| w > 0 && w.checked_add(2).is_some())
                .ok_or("--carry-min-width must be a positive integer with room for the comparator offset (+2)")?;
            synth_opts.lower.carry_min_width = Some(width);
        }
        arch_ice40::synth::synthesize(m, &synth_opts)?
    } else if family == "ecp5" {
        if a.value("--carry-min-width").is_some() {
            return Err("--carry-min-width currently requires --family ice40".into());
        }
        arch_ecp5::synth::synthesize(m, &arch_ecp5::synth::SynthOptions::default())?
    } else {
        if a.value("--carry-min-width").is_some() {
            return Err("--carry-min-width currently requires --family ice40".into());
        }
        arch_xc7::synth::synthesize(m)?
    };
    for l in &log {
        info!("{l}");
    }
    let stats = nl.stats();
    info!("{}", stats.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", "));
    Ok(nl)
}

/// `--timing-weight` (default [`arch_ice40::flow::TIMING_WEIGHT`]).
/// Clock targets: the PCF's `set_frequency` lines, and `--freq` for the
/// other clocks.
pub(crate) fn clocks(
    a: &mut Args,
    pcf: &arch_ice40::pcf::Pcf,
) -> Result<arch_ice40::timing::ClockConstraints, String> {
    let mut c = pcf.clocks.clone();
    c.default_mhz = a.value("--freq").map(|s| s.parse().map_err(|_| "bad --freq")).transpose()?;
    Ok(c)
}

/// `--tries` for iCE40 (default [`arch_ice40::flow::TRIES`]).
fn tries(a: &mut Args) -> Result<usize, String> {
    match a.value("--tries") {
        Some(v) => v.parse().ok().filter(|&n| n >= 1).ok_or("bad --tries".into()),
        None => Ok(arch_ice40::flow::TRIES),
    }
}

pub(crate) fn timing_weight(a: &mut Args) -> Result<f64, String> {
    match a.value("--timing-weight") {
        Some(v) => v
            .parse()
            .ok()
            .filter(|w: &f64| (0.0..=1.0).contains(w))
            .ok_or("bad --timing-weight".into()),
        None => Ok(arch_ice40::flow::TIMING_WEIGHT),
    }
}

fn cmd_synth(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(
        argv,
        &["-I", "-D", "-P", "--top", "--verilog", "--rename", "--family", "--carry-min-width"],
        &["--pack-stats"],
    )?;
    if a.flag("--pack-stats") && !matches!(a.value("--family").as_deref(), Some("artix7" | "xc7")) {
        return Err("--pack-stats currently requires --family artix7".into());
    }
    let nl = synthesize(&mut a)?;
    if a.flag("--pack-stats") {
        xc7::pack_stats(&nl)?;
    }
    if let Some(out) = a.value("--verilog") {
        let name = a.value("--rename").unwrap_or_else(|| {
            nl.top.map(|t| nl.str(t).to_string()).unwrap_or_else(|| "top".into())
        });
        write(&out, synth_netlist::verilog::write(&nl, &name).as_bytes())?;
    }
    Ok(())
}

fn cmd_fmt(argv: &[String]) -> Result<(), String> {
    let a = Args::parse(argv, &[], &["--check"])?;
    let check = a.flag("--check");
    let mut unformatted = Vec::new();
    for i in 0.. {
        let Ok(p) = a.positional(i, "file") else { break };
        let src = read(&p)?;
        let out = synth_weft::fmt::format(&src);
        if out != src {
            if check {
                unformatted.push(p);
            } else {
                write(&p, out.as_bytes())?;
            }
        }
    }
    if unformatted.is_empty() {
        Ok(())
    } else {
        Err(format!("not formatted: {}", unformatted.join(", ")))
    }
}

fn cmd_playground(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(argv, &["--port", "--host", "--xc7-db"], &[])?;
    let port = a.value("--port").unwrap_or_else(|| "8080".into());
    let host = a.value("--host").unwrap_or_else(|| "127.0.0.1".into());
    if let Some(db) = a.value("--xc7-db") {
        synth_playground::XC7_DB.set(db.into()).ok();
    }
    a.finish()?;
    synth_playground::server::serve(&format!("{host}:{port}")).map_err(|e| e.to_string())
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
            "--family",
            "--db",
            "--xdc",
            "--fasm",
            "--package",
            "--pcf",
            "-o",
            "--asc",
            "--seed",
            "--effort",
            "--tries",
            "--carry-min-width",
            "--timing-weight",
            "--freq",
            "--route-timing-rounds",
            "--incremental",
            "--lpf",
            "--speed",
            "--textcfg",
        ],
        &["--timing-allow-fail", "--compress"],
        &["--incremental"],
    )?;
    if let Some(f) = a.value("--family")
        && !matches!(f.as_str(), "ice40" | "artix7" | "xc7" | "ecp5")
    {
        return Err(format!("unknown family `{f}` (expected ice40, ecp5, artix7 or xc7)"));
    }
    if a.value("--family").is_some_and(|f| matches!(f.as_str(), "artix7" | "xc7")) {
        let nl = synthesize(&mut a)?;
        return xc7::finish(nl, &mut a);
    }
    if a.value("--family").is_some_and(|f| f == "ecp5") {
        let nl = synthesize(&mut a)?;
        return ecp5::finish(nl, &mut a);
    }
    for option in ["--lpf", "--speed", "--textcfg"] {
        if a.value(option).is_some() {
            return Err(format!("{option} requires --family ecp5"));
        }
    }
    for option in ["--db", "--xdc", "--fasm"] {
        if a.value(option).is_some() {
            return Err(format!("{option} requires --family artix7"));
        }
    }
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
            .unwrap_or(arch_ice40::flow::ROUTE_TIMING_ROUNDS),
        tries: tries(&mut a)?,
    };
    let res = implement(nl, &pcf.pins, &opts, &mut a)?;
    finish_flow(&res, &mut a)
}

fn cmd_pnr(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse_optional(
        argv,
        &[
            "--device",
            "--family",
            "--db",
            "--xdc",
            "--fasm",
            "--package",
            "--pcf",
            "-o",
            "--asc",
            "--seed",
            "--effort",
            "--tries",
            "--timing-weight",
            "--freq",
            "--route-timing-rounds",
            "--incremental",
        ],
        &["--timing-allow-fail", "--pack-stats"],
        &["--incremental"],
    )?;
    let input = a.positional(0, "netlist")?;
    if let Some(f) = a.value("--family")
        && !matches!(f.as_str(), "ice40" | "artix7" | "xc7")
    {
        return Err(format!("unknown family `{f}` (expected ice40, artix7 or xc7)"));
    }
    if a.value("--family").is_some_and(|f| matches!(f.as_str(), "artix7" | "xc7")) {
        let design = synth_rtlil::parse(&read(&input)?).map_err(|e| e.to_string())?;
        let nl = synth_rtlil::to_netlist(&design, &xc7::Library)?;
        if a.flag("--pack-stats") {
            xc7::pack_stats(&nl)?;
            if a.value("--db").is_none() {
                return Ok(());
            }
        }
        return xc7::finish(nl, &mut a);
    }
    for option in ["--db", "--xdc", "--fasm"] {
        if a.value(option).is_some() {
            return Err(format!("{option} requires --family artix7"));
        }
    }
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
            .unwrap_or(arch_ice40::flow::ROUTE_TIMING_ROUNDS),
        tries: tries(&mut a)?,
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
                info!("ignoring checkpoint {path}: {e}; using fresh PnR");
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
    // Explicit carry thresholds must not reuse a different mapping policy.
    if let Some(width) = a.value("--carry-min-width") {
        key.write(format!("carry-min-width {width}\n").as_bytes());
    }
    key.write(format!("{opts:?}").as_bytes());
    let dir = ".synth/checkpoints";
    std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    let path = format!("{dir}/{}-{}-{:016x}.chk", opts.part, opts.package, key.finish());
    a.set_value("--incremental", path.clone());
    info!("incremental checkpoint: {path}");
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
        info!("{l}");
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
pub(crate) fn input_files(a: &Args) -> Vec<String> {
    (0..).map_while(|i| a.positional(i, "file").ok()).collect()
}

pub(crate) fn is_weft(path: &str) -> bool {
    path.ends_with(".weft")
}

/// Elaborates the design given on the command line, Verilog or Weft, into
/// RTL. Weft sources may be mixed with Verilog files that define their
/// extern modules.
pub(crate) fn load_rtl(a: &mut Args) -> Result<synth_rtl::Module, String> {
    let paths = input_files(a);
    if !paths.iter().any(|p| is_weft(p)) {
        let top_params = a.values("-P").iter().map(|p| parse_param(p)).collect::<Result<_, _>>()?;
        let (sm, src) = load_verilog(a)?;
        let opts = synth_elab::ElabOptions { top: a.value("--top"), top_params };
        let (m, diags) = synth_elab::elaborate(&src, &sm, &opts, &synth_elab::StdBlackBoxes);
        if !diags.items.is_empty() {
            synth_core::progress::eprint(&diags.render(&sm));
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
            synth_core::progress::eprint(&diags.render(&sm));
        }
        if diags.has_errors() {
            return Err(format!("{} error(s)", diags.error_count()));
        }
        Some(src)
    };
    let opts = synth_weft::Options { top: a.value("--top"), params, ..Default::default() };
    let prelude = [("ice40 primitives", arch_ice40::WEFT_PRELUDE)];
    let (m, diags) = synth_weft::compile(&mut sm, &weft, &prelude, src.as_ref(), &opts);
    if !diags.items.is_empty() {
        synth_core::progress::eprint(&diags.render(&sm));
    }
    let warnings =
        diags.items.iter().filter(|d| d.severity == synth_core::Severity::Warning).count();
    match m {
        Some(m) => {
            if warnings > 0 {
                info!("{warnings} warning(s)");
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
        synth_core::progress::eprint(&diags.render(&sm));
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
            info!("merge: {n} duplicate cells");
        }
        "fsm" => {
            let n = synth_rtl::fsm::simplify_state_compares(m);
            info!("fsm: {n} state comparisons simplified");
        }
        "trees" => {
            let n = opt::chains_to_trees(m);
            info!("trees: {n} multiplexer chains as trees");
        }
        "share" => {
            let n = opt::share_arith(m);
            info!("share: {n} multiplexed operations shared");
        }
        "wreduce" => {
            let n = opt::reduce_widths(m);
            info!("wreduce: {n} result bits constant");
        }
        "shiftmul" => {
            let n = opt::shift_by_multiple(m);
            info!("shiftmul: {n} shifts by multiples as multiplexers");
        }
        "dff" => {
            let n = synth_rtl::dff::extract_controls(m);
            info!("dff: {n} flip-flop bits gained controls");
        }
        "memff" => {
            if mem::remove_feedback_writes(m) > 0 {
                opt::clean(m)?;
            }
            mem::merge_write_ports(m);
            let n = mem::merge_read_ffs(m);
            info!("memff: {n} read ports made synchronous");
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
        info!("`{}` checked: no errors", m.name);
        return Ok(());
    }
    let (_, src) = load_verilog(&mut a)?;
    info!("parsed {} module(s)", src.modules.len());
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
    info!(
        "{} wires, cells: {}",
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
