//! The iCE40 primitive library (Lattice iCE technology library names, as used
//! by Yosys `synth_ice40` and in hand-instantiated designs).

use synth_netlist::Dir;

/// Flip-flop features decoded from an `SB_DFF*` cell name.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct DffKind {
    pub neg_clk: bool,
    pub enable: bool,
    /// Has a set or reset input (`R` or `S` port).
    pub has_sr: bool,
    /// The SR input sets (rather than resets) the flop.
    pub set: bool,
    /// The SR input acts asynchronously.
    pub async_sr: bool,
}

/// Decodes `SB_DFF[N][E][SR|R|SS|S]`.
pub fn dff_kind(kind: &str) -> Option<DffKind> {
    let mut rest = kind.strip_prefix("SB_DFF")?;
    let mut k = DffKind::default();
    if let Some(r) = rest.strip_prefix('N') {
        k.neg_clk = true;
        rest = r;
    }
    if let Some(r) = rest.strip_prefix('E') {
        k.enable = true;
        rest = r;
    }
    match rest {
        "" => {}
        "SR" => k.has_sr = true,
        "R" => {
            k.has_sr = true;
            k.async_sr = true;
        }
        "SS" => {
            k.has_sr = true;
            k.set = true;
        }
        "S" => {
            k.has_sr = true;
            k.set = true;
            k.async_sr = true;
        }
        _ => return None,
    }
    Some(k)
}

fn is_ram(kind: &str) -> bool {
    matches!(kind, "SB_RAM40_4K" | "SB_RAM40_4KNR" | "SB_RAM40_4KNW" | "SB_RAM40_4KNRNW")
}

fn is_pll(kind: &str) -> bool {
    matches!(
        kind,
        "SB_PLL40_CORE"
            | "SB_PLL40_PAD"
            | "SB_PLL40_2_PAD"
            | "SB_PLL40_2F_CORE"
            | "SB_PLL40_2F_PAD"
    )
}

/// Strips a `[i]` bit suffix from a pin name.
pub fn port_base(pin: &str) -> &str {
    pin.split_once('[').map_or(pin, |(b, _)| b)
}

/// Direction of `port` (a port name, without bit index) of primitive `kind`.
pub fn port_dir(kind: &str, port: &str) -> Option<Dir> {
    use Dir::*;
    let port = port_base(port);
    match kind {
        "SB_LUT4" => match port {
            "I0" | "I1" | "I2" | "I3" => Some(Input),
            "O" => Some(Output),
            _ => None,
        },
        "SB_CARRY" => match port {
            "I0" | "I1" | "CI" => Some(Input),
            "CO" => Some(Output),
            _ => None,
        },
        _ if dff_kind(kind).is_some() => {
            let k = dff_kind(kind).unwrap();
            match port {
                "C" | "D" => Some(Input),
                "E" if k.enable => Some(Input),
                "R" if k.has_sr && !k.set => Some(Input),
                "S" if k.has_sr && k.set => Some(Input),
                "Q" => Some(Output),
                _ => None,
            }
        }
        _ if is_ram(kind) => match port {
            "RDATA" => Some(Output),
            "RADDR" | "WADDR" | "MASK" | "WDATA" | "RCLKE" | "RCLK" | "RE" | "WCLKE" | "WCLK"
            | "WE" => Some(Input),
            _ => None,
        },
        "SB_IO" | "SB_GB_IO" => match port {
            "PACKAGE_PIN" => Some(Inout),
            "LATCH_INPUT_VALUE" | "CLOCK_ENABLE" | "INPUT_CLK" | "OUTPUT_CLK" | "OUTPUT_ENABLE"
            | "D_OUT_0" | "D_OUT_1" => Some(Input),
            "D_IN_0" | "D_IN_1" => Some(Output),
            "GLOBAL_BUFFER_OUTPUT" if kind == "SB_GB_IO" => Some(Output),
            _ => None,
        },
        "SB_GB" => match port {
            "USER_SIGNAL_TO_GLOBAL_BUFFER" => Some(Input),
            "GLOBAL_BUFFER_OUTPUT" => Some(Output),
            _ => None,
        },
        _ if is_pll(kind) => match port {
            "REFERENCECLK" | "PACKAGEPIN" | "RESETB" | "BYPASS" | "EXTFEEDBACK"
            | "DYNAMICDELAY" | "LATCHINPUTVALUE" | "SCLK" | "SDI" => Some(Input),
            "PLLOUTCORE" | "PLLOUTGLOBAL" | "PLLOUTCOREA" | "PLLOUTCOREB" | "PLLOUTGLOBALA"
            | "PLLOUTGLOBALB" | "LOCK" | "SDO" => Some(Output),
            _ => None,
        },
        "SB_WARMBOOT" => match port {
            "BOOT" | "S1" | "S0" => Some(Input),
            _ => None,
        },
        _ => None,
    }
}

/// [`synth_netlist`] readers use this to learn primitive port directions.
#[derive(Copy, Clone, Debug, Default)]
pub struct Ice40Library;

impl Ice40Library {
    pub fn port_dir(&self, kind: &str, port: &str) -> Option<Dir> {
        port_dir(kind, port)
    }

    /// Whether `kind` names an iCE40 primitive.
    pub fn is_primitive(&self, kind: &str) -> bool {
        matches!(kind, "SB_LUT4" | "SB_CARRY" | "SB_IO" | "SB_GB_IO" | "SB_GB" | "SB_WARMBOOT")
            || dff_kind(kind).is_some()
            || is_ram(kind)
            || is_pll(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dff_names() {
        assert_eq!(dff_kind("SB_DFF"), Some(DffKind::default()));
        let k = dff_kind("SB_DFFNESS").unwrap();
        assert!(k.neg_clk && k.enable && k.has_sr && k.set && !k.async_sr);
        let k = dff_kind("SB_DFFER").unwrap();
        assert!(k.enable && k.has_sr && !k.set && k.async_sr);
        assert_eq!(dff_kind("SB_DFFX"), None);
        assert_eq!(port_dir("SB_DFFER", "R"), Some(Dir::Input));
        assert_eq!(port_dir("SB_DFFER", "S"), None);
        assert_eq!(port_dir("SB_RAM40_4K", "RDATA[3]"), Some(Dir::Output));
    }
}
