//! The ECP5 primitive library (Lattice FPGA library names, as Yosys'
//! `synth_ecp5` and nextpnr-ecp5 use them, and as designs instantiate
//! them).

use synth_netlist::Dir;

pub const LUT4: &str = "LUT4";
pub const PFUMX: &str = "PFUMX";
pub const L6MUX21: &str = "L6MUX21";
pub const CCU2C: &str = "CCU2C";
pub const FF: &str = "TRELLIS_FF";
pub const IO: &str = "TRELLIS_IO";
pub const DP16KD: &str = "DP16KD";
pub const MULT18X18D: &str = "MULT18X18D";
pub const DCCA: &str = "DCCA";
pub const EHXPLLL: &str = "EHXPLLL";

/// Strips a `[i]` bit suffix from a pin name.
pub fn port_base(pin: &str) -> &str {
    pin.split_once('[').map_or(pin, |(b, _)| b)
}

/// A bus pin of a block RAM: `name` and a bit index.
fn bus(port: &str, names: &[&str]) -> bool {
    names.iter().any(|n| {
        port.strip_prefix(n).is_some_and(|r| !r.is_empty() && r.bytes().all(|c| c.is_ascii_digit()))
    })
}

/// Direction of `port` (a port name, with or without bit index) of
/// primitive `kind`.
pub fn port_dir(kind: &str, port: &str) -> Option<Dir> {
    use Dir::*;
    let port = port_base(port);
    match kind {
        LUT4 => match port {
            "A" | "B" | "C" | "D" => Some(Input),
            "Z" => Some(Output),
            _ => None,
        },
        PFUMX => match port {
            "ALUT" | "BLUT" | "C0" => Some(Input),
            "Z" => Some(Output),
            _ => None,
        },
        L6MUX21 => match port {
            "D0" | "D1" | "SD" => Some(Input),
            "Z" => Some(Output),
            _ => None,
        },
        CCU2C => match port {
            "CIN" | "A0" | "B0" | "C0" | "D0" | "A1" | "B1" | "C1" | "D1" => Some(Input),
            "S0" | "S1" | "COUT" => Some(Output),
            _ => None,
        },
        FF => match port {
            "CLK" | "LSR" | "CE" | "DI" | "M" => Some(Input),
            "Q" => Some(Output),
            _ => None,
        },
        IO => match port {
            "B" => Some(Inout),
            "I" | "T" => Some(Input),
            "O" => Some(Output),
            _ => None,
        },
        "BB" => match port {
            "B" => Some(Inout),
            "I" | "T" => Some(Input),
            "O" => Some(Output),
            _ => None,
        },
        "IB" | "OB" => match port {
            "I" => Some(Input),
            "O" => Some(Output),
            _ => None,
        },
        DCCA => match port {
            "CLKI" | "CE" => Some(Input),
            "CLKO" => Some(Output),
            _ => None,
        },
        "OSCG" => (port == "OSC").then_some(Output),
        "GSR" | "SGSR" => (port == "GSR" || port == "CLK").then_some(Input),
        "USRMCLK" => matches!(port, "USRMCLKI" | "USRMCLKTS").then_some(Input),
        DP16KD => {
            if bus(port, &["DIA", "DIB", "ADA", "ADB", "CSA", "CSB"])
                || matches!(
                    port,
                    "CLKA"
                        | "CLKB"
                        | "CEA"
                        | "CEB"
                        | "OCEA"
                        | "OCEB"
                        | "WEA"
                        | "WEB"
                        | "RSTA"
                        | "RSTB"
                )
            {
                Some(Input)
            } else if bus(port, &["DOA", "DOB"]) {
                Some(Output)
            } else {
                None
            }
        }
        MULT18X18D => {
            if bus(port, &["A", "B", "C", "SRIA", "SRIB"])
                || matches!(port, "SIGNEDA" | "SIGNEDB" | "SOURCEA" | "SOURCEB")
                || bus(port, &["CLK", "CE", "RST"])
            {
                Some(Input)
            } else if bus(port, &["P", "ROA", "ROB", "ROC", "SROA", "SROB"]) || port == "SIGNEDP" {
                Some(Output)
            } else {
                None
            }
        }
        EHXPLLL => match port {
            "CLKI" | "CLKFB" | "PHASESEL1" | "PHASESEL0" | "PHASEDIR" | "PHASESTEP"
            | "PHASELOADREG" | "STDBY" | "PLLWAKESYNC" | "RST" | "ENCLKOP" | "ENCLKOS"
            | "ENCLKOS2" | "ENCLKOS3" => Some(Input),
            "CLKOP" | "CLKOS" | "CLKOS2" | "CLKOS3" | "LOCK" | "INTLOCK" | "REFCLK"
            | "CLKINTFB" => Some(Output),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_ports_have_directions() {
        assert_eq!(port_dir(LUT4, "Z"), Some(Dir::Output));
        assert_eq!(port_dir(CCU2C, "COUT"), Some(Dir::Output));
        assert_eq!(port_dir(DP16KD, "ADA13"), Some(Dir::Input));
        assert_eq!(port_dir(DP16KD, "DOB17"), Some(Dir::Output));
        assert_eq!(port_dir(DP16KD, "DO"), None);
        assert_eq!(port_dir(IO, "B[0]"), Some(Dir::Inout));
    }
}
