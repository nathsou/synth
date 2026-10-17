//! Physical constraints files (`.pcf`): `set_io [options] <port> <pin>`
//! and, as nextpnr reads them, `set_frequency <net> <MHz>`.

use crate::place::PinConstraints;
use crate::timing::ClockConstraints;

/// Pin assignments and clock targets of a PCF file.
#[derive(Clone, Debug, Default)]
pub struct Pcf {
    pub pins: PinConstraints,
    pub clocks: ClockConstraints,
}

/// Parses a PCF file.
pub fn parse_pcf(text: &str) -> Result<Pcf, String> {
    let mut pcf = Pcf::default();
    let out = &mut pcf.pins;
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("");
        let toks: Vec<&str> = line.split_whitespace().collect();
        let Some(&cmd) = toks.first() else { continue };
        match cmd {
            "set_io" => {
                // Skip options: flags (`-nowarn`) and options with values
                // (`-pullup yes`, `-io_std SB_LVCMOS`).
                let mut args = Vec::new();
                let mut k = 1;
                while k < toks.len() {
                    match toks[k] {
                        "-nowarn" | "--warn-no-port" => k += 1,
                        t if t.starts_with('-') => k += 2,
                        t => {
                            args.push(t);
                            k += 1;
                        }
                    }
                }
                let [port, pin] = args[..] else {
                    return Err(format!("pcf line {}: expected `set_io <port> <pin>`", i + 1));
                };
                if out.insert(port.to_string(), pin.to_string()).is_some() {
                    return Err(format!("pcf line {}: port {port} constrained twice", i + 1));
                }
            }
            "set_frequency" => {
                let (Some(net), Some(f)) =
                    (toks.get(1), toks.get(2).and_then(|f| f.parse::<f64>().ok()))
                else {
                    return Err(format!(
                        "pcf line {}: expected `set_frequency <net> <MHz>`",
                        i + 1
                    ));
                };
                pcf.clocks.nets.push((net.to_string(), f));
            }
            "set_location" => {}
            other => return Err(format!("pcf line {}: unknown command {other}", i + 1)),
        }
    }
    Ok(pcf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let c = parse_pcf(
            "set_io clk P7 # clock\nset_io -nowarn -pullup yes led[0] J11\n\nset_frequency clk 50\n",
        )
        .unwrap();
        assert_eq!(c.pins.get("clk").map(String::as_str), Some("P7"));
        assert_eq!(c.pins.get("led[0]").map(String::as_str), Some("J11"));
        assert_eq!(c.clocks.nets, vec![("clk".to_string(), 50.0)]);
        assert!(parse_pcf("set_io a").is_err());
    }
}
