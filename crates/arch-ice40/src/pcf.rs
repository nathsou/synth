//! Physical constraints files (`.pcf`): `set_io [options] <port> <pin>`.

use crate::place::PinConstraints;

/// Parses a PCF file into port → pin assignments.
pub fn parse_pcf(text: &str) -> Result<PinConstraints, String> {
    let mut out = PinConstraints::new();
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
            "set_frequency" | "set_location" => {}
            other => return Err(format!("pcf line {}: unknown command {other}", i + 1)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let c =
            parse_pcf("set_io clk P7 # clock\nset_io -nowarn -pullup yes led[0] J11\n\n").unwrap();
        assert_eq!(c.get("clk").map(String::as_str), Some("P7"));
        assert_eq!(c.get("led[0]").map(String::as_str), Some("J11"));
        assert!(parse_pcf("set_io a").is_err());
    }
}
