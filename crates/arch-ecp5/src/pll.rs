//! Phase-locked loops: the generic `synth_pll` primitive lowered onto an
//! `EHXPLLL` (or onto its input clock when the frequencies are equal), the
//! PLL's sites and configuration.
//!
//! A lowered PLL feeds `CLKOP` back internally (`FEEDBK_PATH` `INT_OP`:
//! `CLKINTFB` into `CLKFB`), so that
//! `f_out = f_in / CLKI_DIV * CLKFB_DIV` and the VCO runs at
//! `f_out * CLKOP_DIV`; the dividers are those Project Trellis' `ecppll`
//! finds, the output phase and loop filter its settings (its tiles then
//! configured bit for bit as nextpnr configures `ecppll`'s PLL). Its
//! dynamic phase, standby and output enable inputs are tied off as
//! `ecppll` ties them.

use synth_netlist::{CellId, Const, Dir, Netlist, Param};

use crate::arch::Arch;
use crate::cells::EHXPLLL;
use crate::config::{TileConfig, TileGroup};

/// The family-neutral PLL: `synth_pll<IN_HZ, OUT_HZ>(clk_in, clk_out,
/// locked)`.
pub const GENERIC: &str = "synth_pll";
/// The parameter holding a lowered PLL's output frequency (Hz), the target
/// of the clock it drives.
pub const OUT_HZ: &str = "SYNTH_OUT_HZ";

/// Dividers of a PLL in internal feedback from `CLKOP`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub clki_div: u32,
    pub clkfb_div: u32,
    pub clkop_div: u32,
    /// The frequencies these settings give (Hz).
    pub out_hz: f64,
    pub vco_hz: f64,
}

/// The settings that come closest to `out_hz` from a reference of `in_hz`
/// (ties to the VCO nearest 600 MHz), within the PLL's limits: reference
/// 8-400 MHz, phase detector 3.125-400 MHz, VCO 400-800 MHz, output
/// 3.125-400 MHz.
pub fn settings(in_hz: f64, out_hz: f64) -> Result<Settings, String> {
    let (fin, fout) = (in_hz / 1e6, out_hz / 1e6);
    if !(8.0..=400.0).contains(&fin) {
        return Err(format!("PLL reference of {fin} MHz is outside 8-400 MHz"));
    }
    if !(3.125..=400.0).contains(&fout) {
        return Err(format!("PLL output of {fout} MHz is outside 3.125-400 MHz"));
    }
    let mut best: Option<Settings> = None;
    for clki_div in 1..=128u32 {
        let fpfd = fin / f64::from(clki_div);
        if !(3.125..=400.0).contains(&fpfd) {
            continue;
        }
        for clkfb_div in 1..=80u32 {
            let f = fpfd * f64::from(clkfb_div);
            for clkop_div in 1..=128u32 {
                let vco = f * f64::from(clkop_div);
                if !(400.0..=800.0).contains(&vco) {
                    continue;
                }
                let better = best.is_none_or(|b| {
                    let (e, eb) = ((f - fout).abs(), (b.out_hz / 1e6 - fout).abs());
                    e < eb || (e == eb && (vco - 600.0).abs() < (b.vco_hz / 1e6 - 600.0).abs())
                });
                if better {
                    let (out_hz, vco_hz) = (f * 1e6, vco * 1e6);
                    best = Some(Settings { clki_div, clkfb_div, clkop_div, out_hz, vco_hz });
                }
            }
        }
    }
    best.ok_or_else(|| format!("no PLL setting makes {fout} MHz from {fin} MHz"))
}

/// Largest relative error accepted between the frequency asked for and the
/// one the PLL gives (designs derive baud rates from the former).
const TOLERANCE: f64 = 0.002;

/// An integer parameter (or attribute): a number, bits, or a decimal
/// string as Lattice's library takes them.
fn int_param(nl: &Netlist, c: CellId, name: &str) -> Option<u64> {
    let attr = || {
        let cell = &nl.cells[c];
        cell.attrs.iter().find(|(k, _)| nl.str(**k) == name).map(|(_, v)| v)
    };
    match nl.param(c, name).or_else(attr)? {
        Param::Str(s) => s.trim().parse().ok(),
        p => p.as_u64(),
    }
}

fn str_param<'a>(nl: &'a Netlist, c: CellId, name: &str, default: &'a str) -> &'a str {
    nl.param(c, name).and_then(Param::as_str).unwrap_or(default)
}

/// Lowers every `synth_pll` onto an `EHXPLLL` in internal feedback, or
/// onto its input clock when `IN_HZ` equals `OUT_HZ` (`locked` then high).
/// Standby is tied low through the fabric (its input only ties high); the
/// other control inputs are left for the configuration to tie.
pub fn lower(nl: &mut Netlist, log: &mut Vec<String>) -> Result<(), String> {
    let plls: Vec<CellId> = nl.live_cells().filter(|&c| nl.cell_kind(c) == GENERIC).collect();
    for c in plls {
        let name = nl.str(nl.cells[c].name).to_string();
        let (Some(in_hz), Some(out_hz)) = (int_param(nl, c, "IN_HZ"), int_param(nl, c, "OUT_HZ"))
        else {
            return Err(format!("{GENERIC} {name} needs IN_HZ and OUT_HZ"));
        };
        let clk_in = nl.pin_net(c, "clk_in");
        let clk_out = nl.pin_net(c, "clk_out");
        let locked = nl.pin_net(c, "locked");
        nl.remove_cell(c);
        if in_hz == out_hz {
            if let (Some(i), Some(o)) = (clk_in, clk_out) {
                nl.merge_nets(o, i).map_err(|e| e.0)?;
            }
            if let Some(l) = locked {
                let one = nl.add_const_net(Const::One);
                nl.merge_nets(l, one).map_err(|e| e.0)?;
            }
            log.push(format!("{name}: {} MHz is the input clock, no PLL", out_hz as f64 / 1e6));
            continue;
        }
        let s = settings(in_hz as f64, out_hz as f64)?;
        if (s.out_hz - out_hz as f64).abs() > TOLERANCE * out_hz as f64 {
            return Err(format!(
                "{name}: the PLL cannot make {} MHz from {} MHz (nearest {:.3} MHz)",
                out_hz as f64 / 1e6,
                in_hz as f64 / 1e6,
                s.out_hz / 1e6
            ));
        }
        let pll = nl.add_cell(&name, EHXPLLL);
        let zero = nl.add_const_net(Const::Zero);
        let feedback = nl.add_net(&format!("{name}$feedback"));
        let pins = [
            ("CLKI", Dir::Input, clk_in),
            ("CLKFB", Dir::Input, Some(feedback)),
            ("CLKINTFB", Dir::Output, Some(feedback)),
            ("CLKOP", Dir::Output, clk_out),
            ("LOCK", Dir::Output, locked),
            ("STDBY", Dir::Input, Some(zero)),
        ];
        for (pin, dir, net) in pins {
            nl.add_pin(pll, pin, dir, net).map_err(|e| e.0)?;
        }
        let ints = [
            ("CLKI_DIV", s.clki_div),
            ("CLKFB_DIV", s.clkfb_div),
            ("CLKOP_DIV", s.clkop_div),
            // Half a period, as `ecppll` shifts the output.
            ("CLKOP_CPHASE", (s.clkop_div - 1) / 2),
            ("CLKOP_FPHASE", 0),
        ];
        for (k, v) in ints {
            nl.set_param(pll, k, Param::Int(i64::from(v)));
        }
        nl.set_param(pll, "FEEDBK_PATH", Param::Str("INT_OP".into()));
        nl.set_param(pll, OUT_HZ, Param::Int(s.out_hz.round() as i64));
        log.push(format!(
            "{name}: {:.3} MHz from {} MHz (CLKI_DIV {}, CLKFB_DIV {}, CLKOP_DIV {}, VCO {:.0} MHz)",
            s.out_hz / 1e6,
            in_hz as f64 / 1e6,
            s.clki_div,
            s.clkfb_div,
            s.clkop_div,
            s.vco_hz / 1e6
        ));
    }
    Ok(())
}

/// The clock targets with the output of each lowered PLL added, at the
/// frequency it makes (by the name of the net it drives).
pub fn clock_targets(
    nl: &Netlist,
    clocks: &crate::timing::ClockConstraints,
) -> crate::timing::ClockConstraints {
    let mut out = clocks.clone();
    for c in nl.live_cells().filter(|&c| nl.cell_kind(c) == EHXPLLL) {
        let (Some(hz), Some(net)) = (int_param(nl, c, OUT_HZ), nl.pin_net(c, "CLKOP")) else {
            continue;
        };
        let name = nl.str(nl.nets[net].name).to_string();
        if !out.nets.iter().any(|(n, _)| *n == name) {
            out.nets.push((name, hz as f64 / 1e6));
        }
    }
    out
}

/// The PLLs' corners, by the type of the tile holding the first part of
/// their settings, and where the PLL is from that tile.
const CORNERS: [(&str, i64, i64); 4] =
    [("PLL0_UL", 1, 0), ("PLL0_LL", 0, -1), ("PLL0_LR", 0, -1), ("PLL0_UR", -1, 0)];

/// The PLL sites of a device (`(x, y)`, as Trellis places their bels).
pub fn sites(tiles: &[crate::db::TileInfo]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for t in tiles {
        if let Some(&(_, dx, dy)) = CORNERS.iter().find(|c| c.0 == t.ttype) {
            out.push(((t.col as i64 + dx) as usize, (t.row as i64 + dy) as usize));
        }
    }
    out.sort_unstable();
    out
}

/// The tiles holding the settings of the PLL at `(x, y)` (as nextpnr's
/// `get_pll_tiles`).
pub(crate) fn tiles(arch: &Arch, x: usize, y: usize) -> Result<Vec<String>, String> {
    let at = |dx: i64, dy: i64, types: &[&str]| -> Result<String, String> {
        let (tx, ty) = ((x as i64 + dx) as usize, (y as i64 + dy) as usize);
        arch.dev
            .tile_of_type(ty, tx, types)
            .map(|t| t.name.clone())
            .ok_or_else(|| format!("no {types:?} tile at R{ty}C{tx}"))
    };
    for &(ttype, dx, dy) in &CORNERS {
        let Ok(first) = at(-dx, -dy, &[ttype]) else { continue };
        let second = match ttype {
            "PLL0_UL" => at(-1, 1, &["PLL1_UL"])?,
            "PLL0_LL" => at(1, 1, &["BANKREF8"])?,
            "PLL0_LR" => at(-1, 1, &["PLL1_LR", "BANKREF4"])?,
            _ => at(1, 1, &["PLL1_UR"])?,
        };
        return Ok(vec![first, second]);
    }
    Err(format!("no PLL at R{y}C{x}"))
}

/// The control inputs a configuration ties where nothing drives them, and
/// their values (`ecppll`'s), unless a `<pin>MUX` parameter says otherwise.
const CONTROLS: [(&str, bool); 11] = [
    ("RST", false),
    ("PHASESEL0", false),
    ("PHASESEL1", false),
    ("PHASEDIR", true),
    ("PHASESTEP", true),
    ("PHASELOADREG", true),
    ("PLLWAKESYNC", false),
    ("ENCLKOP", false),
    ("ENCLKOS", false),
    ("ENCLKOS2", false),
    ("ENCLKOS3", false),
];

/// Whether a PLL input is a control input the configuration can tie.
pub fn is_control(pin: &str) -> bool {
    CONTROLS.iter().any(|c| c.0 == pin)
}

/// The settings of PLL `c` at `(x, y)`, as nextpnr's `write_pll` writes
/// them (the loop filter `ecppll`'s where no attribute sets it). Control
/// inputs nothing drives are tied (added to `ties`).
pub(crate) fn config(
    arch: &Arch,
    nl: &Netlist,
    c: CellId,
    (x, y): (usize, usize),
    ties: &mut Vec<(u32, bool)>,
) -> Result<TileGroup, String> {
    let int = |k: &str, default: u64| int_param(nl, c, k).unwrap_or(default);
    let word = |cfg: &mut TileConfig, k: &str, v: u64, n: usize| {
        cfg.add_word(k, (0..n).map(|i| v >> i & 1 == 1).collect());
    };
    let mut cfg = TileConfig::default();
    cfg.add_enum("MODE", "EHXPLLL");
    word(&mut cfg, "CLKI_DIV", int("CLKI_DIV", 1) - 1, 7);
    word(&mut cfg, "CLKFB_DIV", int("CLKFB_DIV", 1) - 1, 7);
    for out in ["CLKOP", "CLKOS", "CLKOS2", "CLKOS3"] {
        let k = format!("{out}_ENABLE");
        cfg.add_enum(&k, str_param(nl, c, &k, "ENABLED"));
    }
    for out in ["CLKOP", "CLKOS", "CLKOS2", "CLKOS3"] {
        word(&mut cfg, &format!("{out}_DIV"), int(&format!("{out}_DIV"), 8) - 1, 7);
        word(&mut cfg, &format!("{out}_CPHASE"), int(&format!("{out}_CPHASE"), 0), 7);
        word(&mut cfg, &format!("{out}_FPHASE"), int(&format!("{out}_FPHASE"), 0), 3);
    }
    cfg.add_enum("FEEDBK_PATH", str_param(nl, c, "FEEDBK_PATH", "CLKOP"));
    for out in ["CLKOP", "CLKOS"] {
        let k = format!("{out}_TRIM_POL");
        cfg.add_enum(&k, str_param(nl, c, &k, "RISING"));
        let k = format!("{out}_TRIM_DELAY");
        cfg.add_enum(&k, &int(&k, 0).to_string());
    }
    let used = nl.pin_net(c, "CLKOP").is_some();
    for (m, div) in ["A", "B", "C", "D"].into_iter().zip(["DIVA", "DIVB", "DIVC", "DIVD"]) {
        let k = format!("OUTDIVIDER_MUX{m}");
        cfg.add_enum(&k, str_param(nl, c, &k, if used { div } else { "REFCLK" }));
    }
    word(&mut cfg, "PLL_LOCK_MODE", int("PLL_LOCK_MODE", 0), 3);
    for (k, default) in [
        ("STDBY_ENABLE", "DISABLED"),
        ("REFIN_RESET", "DISABLED"),
        ("SYNC_ENABLE", "DISABLED"),
        ("INT_LOCK_STICKY", "ENABLED"),
        ("DPHASE_SOURCE", "DISABLED"),
        ("PLLRST_ENA", "DISABLED"),
        ("INTFB_WAKE", "DISABLED"),
    ] {
        cfg.add_enum(k, str_param(nl, c, k, default));
    }
    for (k, default, n) in [
        ("KVCO", 0, 3),
        ("LPF_CAPACITOR", 0, 2),
        ("LPF_RESISTOR", 8, 7),
        ("ICP_CURRENT", 12, 5),
        ("FREQ_LOCK_ACCURACY", 0, 2),
        ("MFG_GMC_GAIN", 0, 3),
        ("MFG_GMC_TEST", 14, 4),
        ("MFG1_TEST", 0, 3),
        ("MFG2_TEST", 0, 3),
        ("MFG_FORCE_VFILTER", 0, 1),
        ("MFG_ICP_TEST", 0, 1),
        ("MFG_EN_UP", 0, 1),
        ("MFG_FLOAT_ICP", 0, 1),
        ("MFG_GMC_PRESET", 0, 1),
        ("MFG_LF_PRESET", 0, 1),
        ("MFG_GMC_RESET", 0, 1),
        ("MFG_LF_RESET", 0, 1),
        ("MFG_LF_RESGRND", 0, 1),
        ("MFG_GMCREF_SEL", 2, 2),
        ("MFG_ENABLE_FILTEROPAMP", 1, 1),
    ] {
        word(&mut cfg, k, int(k, default), n);
    }
    for (pin, default) in CONTROLS {
        if nl.pin_net(c, pin).is_some() {
            continue;
        }
        let value = match nl.param(c, &format!("{pin}MUX")).and_then(Param::as_str) {
            Some(v) => v == "1",
            None => default,
        };
        ties.push((arch.node(x, y, &format!("J{pin}_PLL"))?, value));
    }
    Ok(TileGroup { tiles: tiles(arch, x, y)?, config: cfg })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dividers_match_ecppll() {
        // (in MHz, out MHz, CLKI_DIV, CLKFB_DIV, CLKOP_DIV) from `ecppll`.
        for (fin, fout, i, f, o) in [
            (25.0, 50.0, 1, 2, 12),
            (100.0, 40.0, 5, 2, 15),
            (12.0, 48.0, 1, 4, 12),
            (25.0, 125.0, 1, 5, 5),
            (100.0, 25.0, 4, 1, 24),
            (100.0, 66.666_667, 3, 2, 9),
        ] {
            let s = settings(fin * 1e6, fout * 1e6).unwrap();
            assert_eq!((s.clki_div, s.clkfb_div, s.clkop_div), (i, f, o), "{fin} -> {fout}");
            assert!((s.out_hz - fout * 1e6).abs() < 1.0, "{fout}: {s:?}");
        }
        assert!(settings(4e6, 50e6).is_err());
        assert!(settings(25e6, 500e6).is_err());
    }
}
