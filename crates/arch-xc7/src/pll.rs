//! Phase-locked loops: the generic `synth_pll` primitive lowered onto a
//! `PLLE2_ADV` (or onto its input clock when the frequencies are equal),
//! and the PLL's configuration features.
//!
//! The feedback multiplier is always 8: the PLL's lock and loop-filter
//! settings (`LKTABLE`, `TABLE`) depend on it, and the values nextpnr-xilinx
//! writes, the only ones known here, are those for 8 (in Xilinx's lock table
//! layout, its lock delays are 22). From a 100-200 MHz reference that puts
//! the VCO at 800-1600 MHz, and CLKOUT0 divides it by 1-128.

use std::collections::BTreeSet;

use synth_netlist::{CellId, Const, Dir, Netlist, Param};

use crate::device::Device;
use crate::place::Site;

/// The family-neutral PLL: `synth_pll<IN_HZ, OUT_HZ>(clk_in, clk_out,
/// locked)`.
pub const GENERIC: &str = "synth_pll";
pub const PLL: &str = "PLLE2_ADV";
/// The parameter holding a lowered PLL's output frequency (Hz), the target
/// of the clock it drives.
pub const OUT_HZ: &str = "SYNTH_OUT_HZ";

const MULT: u32 = 8;
/// LKTABLE and TABLE for a feedback multiplier of 8, as nextpnr-xilinx
/// writes them.
const LKTABLE: u64 = 0xB5_BE8F_A401;
const TABLE: u64 = 0x3B4;

/// Dividers of a PLL in internal feedback: `f_out = f_in * 8 / (D * O)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub divclk: u32,
    pub clkout0: u32,
    pub out_hz: f64,
}

/// The settings closest to `out_hz` from a reference of `in_hz`, within
/// the speed grade -1 limits (phase detector 19-450 MHz, VCO 800-1600 MHz).
pub fn settings(in_hz: f64, out_hz: f64) -> Result<Settings, String> {
    let mut best: Option<Settings> = None;
    for divclk in 1..=56 {
        let pfd = in_hz / f64::from(divclk);
        let vco = pfd * f64::from(MULT);
        if !(19e6..=450e6).contains(&pfd) || !(800e6..=1600e6).contains(&vco) {
            continue;
        }
        let clkout0 = ((vco / out_hz).round() as u32).clamp(1, 128);
        let f = vco / f64::from(clkout0);
        if best.is_none_or(|b| (f - out_hz).abs() < (b.out_hz - out_hz).abs()) {
            best = Some(Settings { divclk, clkout0, out_hz: f });
        }
    }
    best.ok_or_else(|| {
        format!(
            "no PLL setting with a feedback multiplier of {MULT} takes a {} MHz reference (100-200 MHz)",
            in_hz / 1e6
        )
    })
}

/// Largest relative error accepted between the frequency asked for and the
/// one the PLL gives (designs derive baud rates from the former).
const TOLERANCE: f64 = 0.002;

fn int_param(nl: &Netlist, c: CellId, name: &str) -> Option<u64> {
    nl.param(c, name).and_then(Param::as_u64)
}

/// Lowers every `synth_pll` onto a `PLLE2_ADV` in internal feedback
/// (CLKFBOUT to CLKFBIN), its control inputs tied as nextpnr-xilinx ties
/// them (CLKINSEL high, RST, PWRDWN and the reconfiguration port low), or
/// onto its input clock when `IN_HZ` equals `OUT_HZ` (`locked`
/// then high).
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
        let pll = nl.add_cell(&name, PLL);
        let (zero, one) = (nl.add_const_net(Const::Zero), nl.add_const_net(Const::One));
        let feedback = nl.add_net(&format!("{name}$feedback"));
        let pins = [
            ("CLKIN1", Dir::Input, clk_in),
            ("CLKFBIN", Dir::Input, Some(feedback)),
            ("CLKFBOUT", Dir::Output, Some(feedback)),
            ("CLKOUT0", Dir::Output, clk_out),
            ("LOCKED", Dir::Output, locked),
            ("CLKINSEL", Dir::Input, Some(one)),
            ("RST", Dir::Input, Some(zero)),
            ("PWRDWN", Dir::Input, Some(zero)),
            ("DEN", Dir::Input, Some(zero)),
            ("DWE", Dir::Input, Some(zero)),
        ];
        for (pin, dir, net) in pins {
            nl.add_pin(pll, pin, dir, net).map_err(|e| e.0)?;
        }
        // The reconfiguration port's address and data, low too.
        for (bus, width) in [("DADDR", 7), ("DI", 16)] {
            for i in 0..width {
                nl.add_pin(pll, &format!("{bus}[{i}]"), Dir::Input, Some(zero)).map_err(|e| e.0)?;
            }
        }
        for (k, v) in
            [("CLKFBOUT_MULT", MULT), ("DIVCLK_DIVIDE", s.divclk), ("CLKOUT0_DIVIDE", s.clkout0)]
        {
            nl.set_param(pll, k, Param::Int(i64::from(v)));
        }
        nl.set_param(pll, OUT_HZ, Param::Int(s.out_hz.round() as i64));
        log.push(format!(
            "{name}: {:.3} MHz from {} MHz (CLKFBOUT_MULT {MULT}, DIVCLK_DIVIDE {}, CLKOUT0_DIVIDE {})",
            s.out_hz / 1e6,
            in_hz as f64 / 1e6,
            s.divclk,
            s.clkout0
        ));
    }
    Ok(())
}

/// The PLL's input pins (as site pins: `DADDR[0]` is `DADDR0`) and their
/// nets.
pub fn inputs(nl: &Netlist, c: CellId) -> Vec<(String, synth_netlist::NetId)> {
    nl.cells[c]
        .pins
        .iter()
        .filter(|p| p.dir == Dir::Input)
        .filter_map(|p| Some((nl.str(p.name).replace(['[', ']'], ""), p.net?)))
        .collect()
}

/// Appends `name[i]` for each set bit `i` of the `width`-bit `value`.
fn bits(out: &mut Vec<String>, name: &str, value: u64, width: u32) {
    out.extend((0..width).filter(|i| value >> i & 1 != 0).map(|i| format!("{name}[{i}]")));
}

/// Appends the features of a counter dividing by `divide` (high and low
/// halves, the edge for odd counts; a count of one bypasses the counter).
fn counter(out: &mut Vec<String>, name: &str, divide: u64) {
    let (high, low, no_count) =
        if divide <= 1 { (1, 1, true) } else { (divide / 2, divide - divide / 2, false) };
    let edge = u64::from(high != low);
    if name == "DIVCLK" {
        bits(out, "DIVCLK_DIVCLK_HIGH_TIME", high, 6);
        bits(out, "DIVCLK_DIVCLK_LOW_TIME", low, 6);
        bits(out, "DIVCLK_DIVCLK_EDGE", edge, 1);
        bits(out, "DIVCLK_DIVCLK_NO_COUNT", u64::from(no_count), 1);
    } else {
        bits(out, &format!("{name}_CLKOUT1_OUTPUT_ENABLE"), 1, 1);
        bits(out, &format!("{name}_CLKOUT1_HIGH_TIME"), high, 6);
        bits(out, &format!("{name}_CLKOUT1_LOW_TIME"), low, 6);
        bits(out, &format!("{name}_CLKOUT2_EDGE"), edge, 1);
        bits(out, &format!("{name}_CLKOUT2_NO_COUNT"), u64::from(no_count), 1);
    }
}

/// The configuration features of a placed PLL, as nextpnr-xilinx writes
/// them.
pub fn features(
    db: &Device,
    nl: &Netlist,
    c: CellId,
    site: &Site,
) -> Result<BTreeSet<String>, String> {
    let param = |k: &str| int_param(nl, c, k).unwrap_or(1);
    if param("CLKFBOUT_MULT") != u64::from(MULT) {
        return Err(format!("PLLs need CLKFBOUT_MULT {MULT} (the only lock settings known)"));
    }
    let mut out = vec!["IN_USE".to_string(), "COMPENSATION.Z_ZHOLD_OR_CLKIN_BUF".to_string()];
    counter(&mut out, "DIVCLK", param("DIVCLK_DIVIDE"));
    counter(&mut out, "CLKFBOUT", u64::from(MULT));
    if nl.pin_net(c, "CLKOUT0").is_some() {
        counter(&mut out, "CLKOUT0", param("CLKOUT0_DIVIDE"));
    }
    bits(&mut out, "FILTREG1_RESERVED", 0x8, 12);
    bits(&mut out, "LKTABLE", LKTABLE, 40);
    bits(&mut out, "LOCKREG3_RESERVED", 1, 1);
    bits(&mut out, "TABLE", TABLE, 10);
    let prefix = format!("{}.PLLE2_ADV", db.tiles[site.tile].name);
    Ok(out.into_iter().map(|f| format!("{prefix}.{f}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dividers_from_100_mhz() {
        for (mhz, clkout0) in [(40.0, 20), (50.0, 16), (25.0, 32), (66.666_667, 12), (100.0, 8)] {
            let s = settings(100e6, mhz * 1e6).unwrap();
            assert_eq!((s.divclk, s.clkout0), (1, clkout0), "{mhz}");
            assert!((s.out_hz / 1e6 - mhz).abs() < 1e-3);
        }
        assert!(settings(12e6, 48e6).is_err());
    }
}
