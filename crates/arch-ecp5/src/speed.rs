//! Timing of a speed grade from Project Trellis' timing database: the
//! delay of each class of routing switch (`interconnect.json`) and the
//! delays, setup and hold times of the primitives (`cells.json`).
//!
//! Switches are classed as Trellis classes them when fitting the delays
//! (`timing/util/pip_classes.py`): by the kinds of wire they join (logic
//! outputs, logic inputs, spans of 0 to 6 tiles and their direction) and,
//! for spans, the relative position of the two wires. Carry chain links
//! and the wide-function multiplexer links inside a logic tile are taken
//! as free, as nextpnr takes them.

use std::path::Path;

use synth_core::{FxHashMap, Json};

/// Delays in picoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PipDelay {
    /// Worst case delay.
    pub delay: f32,
    /// Added for each other switch the source wire drives.
    pub fanout: f32,
}

/// A primitive's timing arcs and checks, by configuration (`SLOGICB`,
/// `SCCU2C`, `DP16KD:REGMODE_A=NOREG,REGMODE_B=NOREG`, ...).
#[derive(Clone, Debug, Default)]
pub struct CellTiming {
    /// Worst-case delay from input pin to output pin.
    pub delays: FxHashMap<(String, String), f32>,
    /// Setup and hold of a pin before the clock pin's edge.
    pub setup: FxHashMap<(String, String), (f32, f32)>,
}

#[derive(Clone, Debug, Default)]
pub struct Speed {
    pub grade: String,
    pub pips: FxHashMap<String, PipDelay>,
    pub cells: FxHashMap<String, CellTiming>,
}

/// Delay of switches no class fits (nextpnr's default).
pub const DEFAULT_PIP: PipDelay = PipDelay { delay: 50.0, fanout: 0.0 };

impl Speed {
    /// Speed grade `6`, `7`, `8` or `8_5G` from the database at `root`.
    pub fn load(root: &Path, grade: &str) -> Result<Speed, String> {
        if !["6", "7", "8", "8_5G"].contains(&grade) {
            return Err(format!("unknown speed grade `{grade}` (6, 7, 8 or 8_5G)"));
        }
        let dir = root.join("ECP5").join("timing").join(format!("speed_{grade}"));
        let read = |f: &str| -> Result<Json, String> {
            let p = dir.join(f);
            let t =
                synth_core::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            Json::parse(&t).map_err(|e| format!("{}: {e}", p.display()))
        };
        let mut pips = FxHashMap::default();
        if let Json::Obj(m) = read("interconnect.json")? {
            for (class, d) in m {
                let worst =
                    |k: &str| d.get(k).map(|v| v.as_array()).and_then(|a| a.get(2)?.as_f64());
                // Trellis' fits are of typical boards; nextpnr adds 10%.
                pips.insert(
                    class,
                    PipDelay {
                        delay: (worst("delay").unwrap_or(0.0) * 1.1) as f32,
                        fanout: worst("fanout").unwrap_or(0.0) as f32,
                    },
                );
            }
        }
        let mut cells = FxHashMap::default();
        if let Json::Obj(m) = read("cells.json")? {
            for (cell, entries) in m {
                let mut t = CellTiming::default();
                for e in entries.as_array() {
                    let pin = |k: &str| match e.get(k) {
                        Some(Json::Str(s)) => Some(s.clone()),
                        // An edge-qualified clock: ["posedge", "CLK"].
                        Some(Json::Arr(a)) => a.get(1).and_then(Json::as_str).map(str::to_string),
                        _ => None,
                    };
                    let worst =
                        |k: &str| e.get(k).map(|v| v.as_array()).and_then(|a| a.get(2)?.as_f64());
                    match e.get("type").and_then(Json::as_str) {
                        Some("Width") => {}
                        Some("IOPath") => {
                            let (Some(from), Some(to)) = (pin("from_pin"), pin("to_pin")) else {
                                continue;
                            };
                            let d =
                                worst("rising").unwrap_or(0.0).max(worst("falling").unwrap_or(0.0));
                            let slot = t.delays.entry((from, to)).or_insert(0.0);
                            *slot = slot.max(d as f32);
                        }
                        Some("SetupHold") => {
                            let (Some(p), Some(clk)) = (pin("pin"), pin("clock")) else { continue };
                            t.setup.insert(
                                (p, clk),
                                (
                                    worst("setup").unwrap_or(0.0) as f32,
                                    worst("hold").unwrap_or(0.0) as f32,
                                ),
                            );
                        }
                        _ => {}
                    }
                }
                cells.insert(cell, t);
            }
        }
        Ok(Speed { grade: grade.to_string(), pips, cells })
    }

    /// The delay of a switch from wire `src` at `(src_x, src_y)` to wire
    /// `dst` at `(dst_x, dst_y)` (base names, without position prefixes).
    pub fn pip(
        &self,
        src: &str,
        src_at: (usize, usize),
        dst: &str,
        dst_at: (usize, usize),
    ) -> PipDelay {
        if free_pip(src, dst) {
            return PipDelay::default();
        }
        match pip_class(src, src_at, dst, dst_at) {
            Some(c) => self.pips.get(&c).copied().unwrap_or(DEFAULT_PIP),
            None => DEFAULT_PIP,
        }
    }
}

/// Carry chain links and the wide-function multiplexer links of a logic
/// tile, which the delays of the logic cells include.
fn free_pip(src: &str, dst: &str) -> bool {
    let wide = |w: &str| w.ends_with("_SLICE") && (w.starts_with("F5") || w.starts_with("FX"));
    src.contains("FCO")
        || dst.contains("FCI")
        || wide(src)
        || src.starts_with("FX")
        || dst.starts_with("FXA")
        || dst.starts_with("FXB")
}

/// A logic input (Trellis' `(J?[ABCDM]|CLK|LSR|CE)\d`, at the start).
fn lc_input(w: &str) -> bool {
    let digit_after =
        |p: &str| w.strip_prefix(p).is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()));
    let lut = |r: &str| {
        let b = r.as_bytes();
        b.len() >= 2 && b"ABCDM".contains(&b[0]) && b[1].is_ascii_digit()
    };
    lut(w)
        || w.strip_prefix('J').is_some_and(lut)
        || ["CLK", "LSR", "CE"].iter().any(|p| digit_after(p))
}

/// A logic output: `F` or `Q` (also as `J`), or a `CLK` and a digit.
fn lc_output(w: &str) -> bool {
    let w2 = w.strip_prefix('J').unwrap_or(w);
    w2.starts_with('F')
        || w2.starts_with('Q')
        || w.strip_prefix("CLK").is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
}

/// `span<length><h|v><direction>` of a span wire (`H02E0701`: `span2he`).
fn span(w: &str) -> Option<String> {
    let b = w.as_bytes();
    if b.len() >= 4
        && (b[0] == b'H' || b[0] == b'V')
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
    {
        Some(format!(
            "span{}{}{}",
            b[2] as char,
            (b[0] as char).to_ascii_lowercase(),
            (b[3] as char).to_ascii_lowercase()
        ))
    } else {
        None
    }
}

/// A span wire's alias in a tile other than its own.
fn denormal(w: &str) -> bool {
    ((w.starts_with("H06") || w.starts_with("V06")) && !w.ends_with("03"))
        || ((w.starts_with("H02") || w.starts_with("V02")) && !w.ends_with("01"))
}

/// `_n1e2`: where `b` is from `a`.
fn rel((ax, ay): (usize, usize), (bx, by): (usize, usize)) -> String {
    let mut r = String::new();
    if by < ay {
        r += &format!("n{}", ay - by);
    } else if by > ay {
        r += &format!("s{}", by - ay);
    }
    if bx < ax {
        r += &format!("w{}", ax - bx);
    } else if bx > ax {
        r += &format!("e{}", bx - ax);
    }
    if r.is_empty() { r } else { format!("_{r}") }
}

/// Trellis' class of a switch, `None` where it has none.
pub fn pip_class(
    src: &str,
    src_at: (usize, usize),
    dst: &str,
    dst_at: (usize, usize),
) -> Option<String> {
    if denormal(src) || denormal(dst) {
        return None;
    }
    if src.ends_with("_SLICE") || src.starts_with("MUX") || dst.ends_with("_SLICE") {
        return Some("slice_internal".into());
    }
    if src.ends_with("_EBR") || dst.ends_with("_EBR") {
        return Some("ebr_internal".into());
    }
    if src.contains("TEST") || dst.contains("TEST") {
        return None;
    }
    if ["ALU", "MULT"].iter().any(|k| src.contains(k) || dst.contains(k)) || dst.contains("PRADD") {
        return Some("dsp_internal".into());
    }
    let trim = |w: &str| w[..w.len() - 1].to_ascii_lowercase();
    if lc_input(dst) {
        if lc_output(src) {
            Some(format!("{}_to_{}", trim(src), trim(dst)))
        } else if let Some(s) = span(src) {
            Some(format!("{s}_to_{}{}", trim(dst), rel(src_at, dst_at)))
        } else if src.contains("HPBX") {
            Some(format!("global_to_{}", trim(dst)))
        } else {
            None
        }
    } else if let Some(d) = span(dst) {
        if lc_output(src) {
            Some(format!("{}_to_{d}{}", trim(src), rel(src_at, dst_at)))
        } else if let Some(s) = span(src) {
            Some(format!("{s}_to_{d}{}", rel(src_at, dst_at)))
        } else if src.contains("HPBX") {
            Some(format!("global_to_{d}"))
        } else {
            None
        }
    } else if src.starts_with("LSR") && dst.starts_with("MUXLSR") {
        Some("lsr_to_muxlsr".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_are_classed_as_trellis_classes_them() {
        assert_eq!(pip_class("F3", (5, 5), "A0", (5, 5)).as_deref(), Some("f_to_a"));
        assert_eq!(pip_class("Q1", (5, 5), "H02E0101", (6, 5)).as_deref(), Some("q_to_span2he_e1"));
        assert_eq!(pip_class("V02N0701", (5, 6), "C3", (5, 5)).as_deref(), Some("span2vn_to_c_n1"));
        assert_eq!(
            pip_class("H06W0303", (8, 5), "V02S0201", (5, 4)).as_deref(),
            Some("span6hw_to_span2vs_n1w3")
        );
        assert_eq!(
            pip_class("G_HPBX0100", (0, 0), "CLK0", (5, 5)).as_deref(),
            Some("global_to_clk")
        );
        assert_eq!(pip_class("JF2", (5, 5), "JA0", (5, 5)).as_deref(), Some("jf_to_ja"));
        assert_eq!(pip_class("LSR0", (5, 5), "MUXLSR1", (5, 5)).as_deref(), Some("lsr_to_muxlsr"));
        assert_eq!(pip_class("H02E0101", (5, 5), "JCE0", (5, 5)), None);
        assert_eq!(pip_class("H06W0102", (5, 5), "A0", (5, 5)), None);
        assert!(free_pip("FCO", "E1_HFIE0000") && free_pip("F5A_SLICE", "F0"));
        assert!(!free_pip("F5", "A0"));
    }
}
