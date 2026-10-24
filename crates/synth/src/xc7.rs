//! Native Artix-7 command adapters.
use crate::{args::Args, read, write};
use std::path::Path;
pub struct Library;
impl synth_rtlil::PrimitiveLibrary for Library {
    fn port_dir(&self, kind: &str, port: &str) -> Option<synth_netlist::Dir> {
        use synth_netlist::Dir::{Input, Output};
        match kind {
            k if arch_xc7::pack::is_lut(k) => {
                if port == "O" {
                    Some(Output)
                } else if port.starts_with('I') {
                    Some(Input)
                } else {
                    None
                }
            }
            k if arch_xc7::pack::is_ff(k) => match port {
                "Q" => Some(Output),
                "C" | "D" | "CE" | "R" | "S" | "CLR" | "PRE" => Some(Input),
                _ => None,
            },
            "INV" | "MUXF7" | "MUXF8" => match port {
                "O" => Some(Output),
                "I" | "I0" | "I1" | "S" => Some(Input),
                _ => None,
            },
            "SRL16E" | "SRLC32E" => match port {
                "Q" | "Q15" | "Q31" => Some(Output),
                "A" | "A0" | "A1" | "A2" | "A3" | "CE" | "CLK" | "D" => Some(Input),
                _ => None,
            },
            "RAM32M" | "RAM64M" => match port {
                p if p.starts_with("DO") => Some(Output),
                p if p.starts_with("ADDR") || p.starts_with("DI") || p == "WE" || p == "WCLK" => {
                    Some(Input)
                }
                _ => None,
            },
            "CARRY4" => match port {
                "O" | "CO" => Some(Output),
                "S" | "DI" | "CI" | "CYINIT" => Some(Input),
                _ => None,
            },
            "DSP48E1" => match port {
                "P" | "ACOUT" | "BCOUT" | "CARRYCASCOUT" | "CARRYOUT" | "MULTSIGNOUT"
                | "OVERFLOW" | "PATTERNBDETECT" | "PATTERNDETECT" | "PCOUT" | "UNDERFLOW" => {
                    Some(Output)
                }
                "A" | "ACIN" | "ALUMODE" | "B" | "BCIN" | "C" | "CARRYCASCIN" | "CARRYIN"
                | "CARRYINSEL" | "CEA1" | "CEA2" | "CEAD" | "CEALUMODE" | "CEB1" | "CEB2"
                | "CEC" | "CECARRYIN" | "CECTRL" | "CED" | "CEINMODE" | "CEM" | "CEP" | "CLK"
                | "D" | "INMODE" | "MULTSIGNIN" | "OPMODE" | "PCIN" | "RSTA" | "RSTALLCARRYIN"
                | "RSTALUMODE" | "RSTB" | "RSTC" | "RSTCTRL" | "RSTD" | "RSTINMODE" | "RSTM"
                | "RSTP" => Some(Input),
                _ => None,
            },
            "RAMB18E1" => match port {
                "DOADO" | "DOBDO" | "DOPADOP" | "DOPBDOP" => Some(Output),
                "ADDRARDADDR" | "ADDRBWRADDR" | "CLKARDCLK" | "CLKBWRCLK" | "DIADI" | "DIBDI"
                | "DIPADIP" | "DIPBDIP" | "ENARDEN" | "ENBWREN" | "REGCEAREGCE" | "REGCEB"
                | "RSTRAMARSTRAM" | "RSTRAMB" | "RSTREGARSTREG" | "RSTREGB" | "WEA" | "WEBWE" => {
                    Some(Input)
                }
                _ => None,
            },
            _ => None,
        }
    }
}
/// Prints native packing statistics: slices (and those needing a SLICEM),
/// LUTs including those added by packing, and carry blocks.
pub fn pack_stats(nl: &synth_netlist::Netlist) -> Result<(), String> {
    let mut nl = nl.clone();
    let packed = arch_xc7::pack::pack(&mut nl)?;
    let count = |f: &dyn Fn(&str) -> bool| nl.live_cells().filter(|&c| f(nl.cell_kind(c))).count();
    let luts = count(&|k| arch_xc7::pack::is_lut(k));
    let carries = count(&|k| k == "CARRY4");
    let slicem = packed.slices.iter().filter(|s| s.needs_m(&nl)).count();
    info!(
        "packed {{\"slices\":{},\"slicem\":{slicem},\"lut\":{luts},\"carry4\":{carries}}}",
        packed.slices.len()
    );
    Ok(())
}

pub fn finish(nl: synth_netlist::Netlist, a: &mut Args) -> Result<(), String> {
    for opt in [
        "--pcf",
        "--asc",
        "--package",
        "--incremental",
        "--route-timing-rounds",
        "--timing-weight",
        "--freq",
    ] {
        if a.value(opt).is_some() {
            return Err(format!("{opt} is not supported by the Artix-7 physical flow"));
        }
    }
    let db = a.required("--db")?;
    let part = a.required("--device")?;
    let xdc = arch_xc7::xdc::Constraints::parse(&read(&a.required("--xdc")?)?)?;
    let opts = arch_xc7::flow::FlowOptions {
        part: part.clone(),
        seed: a
            .value("--seed")
            .map(|v| v.parse().map_err(|_| "invalid seed"))
            .transpose()?
            .unwrap_or(1),
        effort: a
            .value("--effort")
            .map(|v| v.parse().map_err(|_| "invalid effort"))
            .transpose()?
            .unwrap_or(1.0),
        tries: a
            .value("--tries")
            .map(|v| v.parse().map_err(|_| "invalid tries"))
            .transpose()?
            .unwrap_or(4),
    };
    let name = nl.top.map(|t| nl.str(t).to_string()).unwrap_or_else(|| "top".into());
    let result = arch_xc7::flow::implement(nl, Path::new(&db), &xdc, &opts)?;
    for l in &result.log {
        info!("{l}");
    }
    if let Some(p) = a.value("--fasm") {
        write(&p, arch_xc7::fasm::text(&result.features).as_bytes())?;
    }
    let out = a.value("-o").unwrap_or_else(|| "out.bit".into());
    let bytes = if out.ends_with(".bit") {
        arch_xc7::bitstream::bit_file(
            &arch_xc7::device::Part::load(Path::new(&db), &part)?,
            &name,
            &result.bitstream,
        )?
    } else {
        result.bitstream
    };
    write(&out, &bytes)?;
    if !a.flag("--timing-allow-fail") {
        return Err("Artix-7 timing is not certified yet; bitstream written, but timing acceptance failed (use --timing-allow-fail for bring-up)".into());
    }
    Ok(())
}
pub fn pack_fasm(argv: &[String]) -> Result<(), String> {
    let mut a = Args::parse(argv, &["--db", "--device", "-o"], &[])?;
    let input = a.positional(0, "FASM input")?;
    let db = a.required("--db")?;
    let part = a.required("--device")?;
    let out = a.required("-o")?;
    let dev = arch_xc7::device::Device::geometry(Path::new(&db), &part)?;
    let features = arch_xc7::fasm::parse(&read(&input)?)?;
    let frames = arch_xc7::fasm::Assembler::new(&dev, Path::new(&db)).assemble(&features)?;
    let raw = arch_xc7::bitstream::encode(&dev.part, &frames, true)?;
    write(
        &out,
        &if out.ends_with(".bit") {
            arch_xc7::bitstream::bit_file(&dev.part, "synth", &raw)?
        } else {
            raw
        },
    )
}
pub fn unpack(argv: &[String]) -> Result<(), String> {
    use std::fmt::Write;
    let mut a = Args::parse(argv, &["--db", "--device", "-o"], &[])?;
    let input = a.positional(0, "bitstream")?;
    let db = a.required("--db")?;
    let part = a.required("--device")?;
    let out = a.required("-o")?;
    let part = arch_xc7::device::Part::load(Path::new(&db), &part)?;
    let bytes = std::fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    let frames = arch_xc7::bitstream::decode(&part, &bytes)?;
    let mut text = String::new();
    for (a, f) in frames {
        write!(text, "0x{a:08x} ").unwrap();
        for (i, w) in f.into_iter().enumerate() {
            if i != 0 {
                text.push(',');
            }
            write!(text, "0x{w:08x}").unwrap();
        }
        text.push('\n');
    }
    write(&out, text.as_bytes())
}
