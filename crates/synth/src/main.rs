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
    pnr        Place and route a mapped netlist (RTLIL) for iCE40
    pack-asc   Convert an IceStorm .asc file to a binary bitstream
    unpack     Convert a binary bitstream to an IceStorm .asc file
    help       Print this message
    version    Print version information

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

fn cmd_pnr(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(
        argv,
        &["--device", "--package", "--pcf", "-o", "--asc", "--seed", "--effort"],
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
    let opts = arch_ice40::flow::FlowOptions { part, package, seed, place_effort: effort };
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

fn cmd_check(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(argv, &["-I", "-D"], &[])?;
    let mut sm = synth_core::SourceMap::new();
    let mut opts = synth_verilog::PreprocOptions::default();
    if let Some(i) = a.value("-I") {
        opts.include_dirs.push(i.into());
    }
    if let Some(d) = a.value("-D") {
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
    eprintln!("info: parsed {} module(s)", src.modules.len());
    Ok(())
}
