//! Lattice preference files (`.lpf`): pin locations, I/O buffer settings,
//! clock frequencies and configuration options, in the subset nextpnr-ecp5
//! reads:
//!
//! ```text
//! LOCATE COMP "led[0]" SITE "B2";
//! IOBUF PORT "led[0]" IO_TYPE=LVCMOS33 DRIVE=8;
//! FREQUENCY PORT "clk" 25 MHZ;
//! SYSCONFIG CONFIG_IOVOLTAGE=3.3 COMPRESS_CONFIG=ON;
//! ```
//!
//! Commands end with `;` and may span lines; `#` and `//` start comments.
//! `BLOCK RESETPATHS` and `BLOCK ASYNCPATHS` are accepted and have no
//! effect, as other commands are ignored with a warning.

use std::collections::BTreeMap;

use synth_core::FxHashMap;

/// I/O buffer settings an `IOBUF` command may give.
pub const IOBUF_KEYS: [&str; 13] = [
    "IO_TYPE",
    "BANK",
    "BANK_VCC",
    "VREF",
    "PULLMODE",
    "DRIVE",
    "SLEWRATE",
    "CLAMP",
    "OPENDRAIN",
    "DIFFRESISTOR",
    "DIFFDRIVE",
    "HYSTERESIS",
    "TERMINATION",
];

const SYSCONFIG_KEYS: [&str; 15] = [
    "SLAVE_SPI_PORT",
    "MASTER_SPI_PORT",
    "SLAVE_PARALLEL_PORT",
    "BACKGROUND_RECONFIG",
    "DONE_EX",
    "DONE_OD",
    "DONE_PULL",
    "MCCLK_FREQ",
    "TRANSFR",
    "CONFIG_IOVOLTAGE",
    "CONFIG_SECURE",
    "WAKE_UP",
    "COMPRESS_CONFIG",
    "CONFIG_MODE",
    "INBUF",
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Constraints {
    /// Package pin of each port bit (`led[0]`).
    pub sites: FxHashMap<String, String>,
    /// I/O buffer settings of each port bit.
    pub iobuf: FxHashMap<String, BTreeMap<String, String>>,
    /// Clock frequency (MHz) of each port or net.
    pub frequencies: BTreeMap<String, f64>,
    pub sysconfig: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}

impl Constraints {
    pub fn parse(text: &str) -> Result<Constraints, String> {
        let mut c = Constraints::default();
        // Commands with the line each ends on.
        let mut commands = Vec::new();
        let mut buf = String::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("");
            let line = line.split("//").next().unwrap_or("");
            buf.push_str(line);
            buf.push(' ');
            while let Some(p) = buf.find(';') {
                commands.push((i + 1, buf[..p].to_string()));
                buf.drain(..=p);
            }
        }
        if !buf.trim().is_empty() {
            return Err(format!("unexpected end of preferences: `{}` has no `;`", buf.trim()));
        }
        for (line, cmd) in commands {
            let w: Vec<&str> = cmd.split_whitespace().collect();
            let err = |m: &str| format!("line {line}: {m}");
            match w.first().copied() {
                None => {}
                Some("BLOCK") if matches!(w.get(1).copied(), Some("ASYNCPATHS" | "RESETPATHS")) => {
                }
                Some("LOCATE") => match w.as_slice() {
                    ["LOCATE", "COMP", port, "SITE", site] => {
                        c.sites.insert(unquote(port).to_string(), unquote(site).to_string());
                    }
                    _ => return Err(err("expected `LOCATE COMP <port> SITE <pin>`")),
                },
                Some("IOBUF") => {
                    if w.get(1) != Some(&"PORT") || w.len() < 3 {
                        return Err(err("expected `IOBUF PORT <port> <attr>=<value>...`"));
                    }
                    let port = unquote(w[2]).to_string();
                    for kv in &w[3..] {
                        let (k, v) =
                            kv.split_once('=').ok_or_else(|| err("expected `<attr>=<value>`"))?;
                        if !IOBUF_KEYS.contains(&k) {
                            c.warnings.push(format!("line {line}: unknown IOBUF setting `{k}`"));
                        }
                        c.iobuf
                            .entry(port.clone())
                            .or_default()
                            .insert(k.to_string(), unquote(v).to_string());
                    }
                }
                Some("FREQUENCY") if matches!(w.get(1).copied(), Some("PORT" | "NET")) => {
                    let (Some(target), Some(value), Some(unit)) = (w.get(2), w.get(3), w.get(4))
                    else {
                        return Err(err("expected `FREQUENCY PORT|NET <name> <value> <unit>`"));
                    };
                    let value: f64 = value.parse().map_err(|_| err("bad frequency"))?;
                    let mhz = match unit.to_ascii_uppercase().as_str() {
                        "MHZ" => value,
                        "KHZ" => value / 1e3,
                        "HZ" => value / 1e6,
                        u => return Err(err(&format!("unsupported frequency unit `{u}`"))),
                    };
                    c.frequencies.insert(unquote(target).to_string(), mhz);
                }
                Some("SYSCONFIG") => {
                    for kv in &w[1..] {
                        let (k, v) = kv
                            .split_once('=')
                            .ok_or_else(|| err("expected `SYSCONFIG <attr>=<value>`"))?;
                        if !SYSCONFIG_KEYS.contains(&k) {
                            return Err(err(&format!("unknown SYSCONFIG setting `{k}`")));
                        }
                        c.sysconfig.insert(k.to_string(), v.to_string());
                    }
                }
                Some(_) => c.warnings.push(format!("line {line}: ignored `{}`", cmd.trim())),
            }
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_are_read() {
        let c = Constraints::parse(
            "# a board\nLOCATE COMP \"clk\" SITE \"P3\";\nIOBUF PORT \"clk\" IO_TYPE=LVCMOS33\n  PULLMODE=UP; // up\n\
             FREQUENCY PORT \"clk\" 25 MHZ;\nBLOCK RESETPATHS;\nSYSCONFIG COMPRESS_CONFIG=ON;\nUSE PRIMARY NET \"clk\";\n",
        )
        .unwrap();
        assert_eq!(c.sites["clk"], "P3");
        assert_eq!(c.iobuf["clk"]["PULLMODE"], "UP");
        assert_eq!(c.frequencies["clk"], 25.0);
        assert_eq!(c.sysconfig["COMPRESS_CONFIG"], "ON");
        assert_eq!(c.warnings.len(), 1);
        assert!(Constraints::parse("LOCATE COMP \"a\" SITE \"B2\"").is_err());
    }
}
