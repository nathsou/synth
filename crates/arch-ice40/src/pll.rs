//! Phase-locked loops: `SB_PLL40_CORE` settings from frequencies (as
//! IceStorm's `icepll` finds them), and the generic `synth_pll` primitive
//! lowered onto the PLL, or onto its input clock when the frequencies are
//! equal.

use synth_netlist::{CellId, Const, Dir, Netlist, Param};

/// The family-neutral PLL: `synth_pll<IN_HZ, OUT_HZ>(clk_in, clk_out,
/// locked)`.
pub const GENERIC: &str = "synth_pll";
/// The PLL fed from the fabric.
pub const CORE: &str = "SB_PLL40_CORE";
/// The parameter holding a lowered PLL's output frequency (Hz), the target
/// of the clock it drives.
pub const OUT_HZ: &str = "SYNTH_OUT_HZ";

/// Settings of a PLL in SIMPLE feedback mode:
/// `f_out = f_in * (DIVF + 1) / ((DIVR + 1) * 2^DIVQ)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub divr: u8,
    pub divf: u8,
    pub divq: u8,
    pub filter_range: u8,
    /// The frequency these settings give (Hz).
    pub out_hz: f64,
}

/// The settings that come closest to `out_hz` from a reference of `in_hz`,
/// within the PLL's limits (reference and phase detector 10-133 MHz, VCO
/// 533-1066 MHz, output 16-275 MHz).
pub fn settings(in_hz: f64, out_hz: f64) -> Result<Settings, String> {
    let (fin, fout) = (in_hz / 1e6, out_hz / 1e6);
    if !(10.0..=133.0).contains(&fin) {
        return Err(format!("PLL reference of {fin} MHz is outside 10-133 MHz"));
    }
    if !(16.0..=275.0).contains(&fout) {
        return Err(format!("PLL output of {fout} MHz is outside 16-275 MHz"));
    }
    let mut best: Option<Settings> = None;
    for divr in 0..=15u8 {
        let fpfd = fin / f64::from(divr + 1);
        if !(10.0..=133.0).contains(&fpfd) {
            continue;
        }
        for divf in 0..=127u8 {
            let fvco = fpfd * f64::from(divf + 1);
            if !(533.0..=1066.0).contains(&fvco) {
                continue;
            }
            for divq in 1..=6u8 {
                let f = fvco / f64::from(1u32 << divq);
                if best.is_none_or(|b| (f - fout).abs() < (b.out_hz / 1e6 - fout).abs()) {
                    let filter_range = match fpfd {
                        x if x < 17.0 => 1,
                        x if x < 26.0 => 2,
                        x if x < 44.0 => 3,
                        x if x < 66.0 => 4,
                        x if x < 101.0 => 5,
                        _ => 6,
                    };
                    best = Some(Settings { divr, divf, divq, filter_range, out_hz: f * 1e6 });
                }
            }
        }
    }
    best.ok_or_else(|| format!("no PLL setting makes {fout} MHz from {fin} MHz"))
}

/// Largest relative error accepted between the frequency asked for and the
/// one the PLL gives (designs derive baud rates from the former).
const TOLERANCE: f64 = 0.002;

fn int_param(nl: &Netlist, c: CellId, name: &str) -> Option<u64> {
    nl.param(c, name).and_then(Param::as_u64)
}

/// Lowers every `synth_pll` onto an `SB_PLL40_CORE` (its output on a global
/// network, `RESETB` tied high), or onto its input clock when `IN_HZ` equals
/// `OUT_HZ` (`locked` then high).
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
        let pll = nl.add_cell(&name, CORE);
        let one = nl.add_const_net(Const::One);
        nl.add_pin(pll, "REFERENCECLK", Dir::Input, clk_in).map_err(|e| e.0)?;
        nl.add_pin(pll, "RESETB", Dir::Input, Some(one)).map_err(|e| e.0)?;
        nl.add_pin(pll, "PLLOUTGLOBAL", Dir::Output, clk_out).map_err(|e| e.0)?;
        nl.add_pin(pll, "LOCK", Dir::Output, locked).map_err(|e| e.0)?;
        for (k, v) in
            [("DIVR", s.divr), ("DIVF", s.divf), ("DIVQ", s.divq), ("FILTER_RANGE", s.filter_range)]
        {
            nl.set_param(pll, k, Param::Int(i64::from(v)));
        }
        nl.set_param(pll, "FEEDBACK_PATH", Param::Str("SIMPLE".into()));
        nl.set_param(pll, "PLLOUT_SELECT", Param::Str("GENCLK".into()));
        nl.set_param(pll, OUT_HZ, Param::Int(s.out_hz.round() as i64));
        log.push(format!(
            "{name}: {:.3} MHz from {} MHz (DIVR {}, DIVF {}, DIVQ {}, FILTER_RANGE {})",
            s.out_hz / 1e6,
            in_hz as f64 / 1e6,
            s.divr,
            s.divf,
            s.divq,
            s.filter_range
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
    for c in nl.live_cells().filter(|&c| nl.cell_kind(c) == CORE) {
        let (Some(hz), Some(net)) = (int_param(nl, c, OUT_HZ), nl.pin_net(c, "PLLOUTGLOBAL"))
        else {
            continue;
        };
        out.nets.insert(0, (nl.str(nl.nets[net].name).to_string(), hz as f64 / 1e6));
    }
    out
}

/// The value of an integer PLL parameter (a number, bits or a binary
/// string), 0 if absent.
pub fn param(nl: &Netlist, c: CellId, name: &str) -> u64 {
    int_param(nl, c, name).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_match_icepll() {
        // (in MHz, out MHz, DIVR, DIVF, DIVQ, FILTER_RANGE) from `icepll`.
        for (fin, fout, r, f, q, fr) in [
            (12.0, 48.0, 0, 63, 4, 1),
            (12.0, 30.0, 0, 79, 5, 1),
            (25.0, 50.0, 0, 31, 4, 2),
            (100.0, 25.0, 0, 7, 5, 5),
            (100.0, 40.0, 4, 31, 4, 2),
            (100.0, 50.0, 0, 7, 4, 5),
            (100.0, 80.0, 4, 31, 3, 2),
        ] {
            let s = settings(fin * 1e6, fout * 1e6).unwrap();
            assert_eq!((s.divr, s.divf, s.divq, s.filter_range), (r, f, q, fr), "{fin} -> {fout}");
            assert_eq!(s.out_hz, fout * 1e6);
        }
        // Several settings are exact: any will do, within the limits.
        for (fin, fout) in [(100.0, 66.666_667), (100.0, 33.333_333), (100.0, 57.142_857)] {
            let s = settings(fin * 1e6, fout * 1e6).unwrap();
            assert!((s.out_hz / 1e6 - fout).abs() < 1e-3, "{fout}: {s:?}");
            let fpfd = fin / f64::from(s.divr + 1);
            let fvco = fpfd * f64::from(s.divf + 1);
            assert!((10.0..=133.0).contains(&fpfd) && (533.0..=1066.0).contains(&fvco));
        }
        // `icepll -i 12 -o 25` gives 25.125 MHz, too far for a baud rate.
        assert_eq!(settings(12e6, 25e6).unwrap().out_hz, 25.125e6);
        assert!(settings(100e6, 300e6).is_err());
    }
}
