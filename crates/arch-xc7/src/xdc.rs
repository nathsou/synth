//! Explicitly supported XDC subset: set_property, get_ports and create_clock.
//! No Tcl is executed. Unknown active commands fail with a line diagnostic.
use std::collections::BTreeMap;
#[derive(Clone, Debug, Default)]
pub struct Port {
    pub pin: Option<String>,
    pub standard: Option<String>,
    pub drive: Option<u32>,
    pub slew: Option<String>,
    pub pull: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct Constraints {
    pub ports: BTreeMap<String, Port>,
    pub clocks: BTreeMap<String, f64>,
}
impl Constraints {
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut c = Self::default();
        let s = s.replace("\\\n", " ");
        for (line_no, line) in s.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let words = words(line).map_err(|e| format!("XDC line {}: {e}", line_no + 1))?;
            if words.is_empty() {
                continue;
            }
            let err = |s: &str| format!("XDC line {}: {s}", line_no + 1);
            match words[0].as_str() {
                "set_property" => {
                    if words.len() != 4 {
                        return Err(err("expected set_property <name> <value> [get_ports {...}]"));
                    }
                    let ports = get_ports(&words[3]).map_err(|e| err(&e))?;
                    let props = if words[1] == "-dict" {
                        words_inner(&words[2])?
                    } else {
                        vec![words[1].clone(), unquote(&words[2]).into()]
                    };
                    if !props.len().is_multiple_of(2) {
                        return Err(err("property dictionary must contain pairs"));
                    }
                    for name in ports {
                        let p = c.ports.entry(name).or_default();
                        for pair in props.as_chunks::<2>().0 {
                            let v = unquote(&pair[1]).to_string();
                            match pair[0].as_str() {
                                "PACKAGE_PIN" => p.pin = Some(v),
                                "IOSTANDARD" => p.standard = Some(v),
                                "DRIVE" => {
                                    p.drive = Some(v.parse().map_err(|_| err("invalid DRIVE"))?)
                                }
                                "SLEW" => p.slew = Some(v),
                                "PULLTYPE" => p.pull = Some(v),
                                _ => {
                                    return Err(err(&format!(
                                        "unsupported port property {}",
                                        pair[0]
                                    )));
                                }
                            }
                        }
                    }
                }
                "create_clock" => {
                    let mut period = None;
                    let mut waveform_fall = None;
                    let mut ports = None;
                    let mut i = 1;
                    while i < words.len() {
                        match words[i].as_str() {
                            "-period" => {
                                i += 1;
                                period = Some(
                                    words
                                        .get(i)
                                        .ok_or_else(|| err("missing clock period"))?
                                        .parse::<f64>()
                                        .map_err(|_| err("invalid clock period"))?,
                                );
                            }
                            "-name" => {
                                i += 1;
                                if i >= words.len() {
                                    return Err(err("missing clock name"));
                                }
                            }
                            "-waveform" => {
                                i += 1;
                                let w = words_inner(
                                    words.get(i).ok_or_else(|| err("missing waveform"))?,
                                )?;
                                if w.len() != 2 {
                                    return Err(err("invalid waveform"));
                                }
                                let rise: f64 =
                                    w[0].parse().map_err(|_| err("invalid waveform"))?;
                                let fall: f64 =
                                    w[1].parse().map_err(|_| err("invalid waveform"))?;
                                waveform_fall = Some(fall);
                                if rise != 0.0 || !fall.is_finite() || fall <= 0.0 {
                                    return Err(err(
                                        "only zero-phase rising-edge clocks are supported",
                                    ));
                                }
                            }
                            w if w.starts_with('[') => {
                                ports = Some(get_ports(w).map_err(|e| err(&e))?)
                            }
                            _ => return Err(err("unsupported create_clock argument")),
                        }
                        i += 1;
                    }
                    let period = period.ok_or_else(|| err("clock needs -period"))?;
                    if !period.is_finite() || period <= 0.0 {
                        return Err(err("clock period must be positive and finite"));
                    }
                    if waveform_fall.is_some_and(|f| (f - period / 2.0).abs() > 1e-9) {
                        return Err(err("only 50% duty-cycle clocks are supported"));
                    }
                    for port in ports.ok_or_else(|| err("clock needs get_ports target"))? {
                        if c.clocks.insert(port, period).is_some() {
                            return Err(err("duplicate clock constraint"));
                        }
                    }
                }
                _ => return Err(err(&format!("unsupported command {}", words[0]))),
            }
        }
        Ok(c)
    }
}
fn unquote(s: &str) -> &str {
    if (s.starts_with('{') && s.ends_with('}')) || (s.starts_with('"') && s.ends_with('"')) {
        &s[1..s.len() - 1]
    } else {
        s
    }
}
fn words_inner(s: &str) -> Result<Vec<String>, String> {
    words(unquote(s))
}
fn get_ports(s: &str) -> Result<Vec<String>, String> {
    let s =
        s.strip_prefix('[').and_then(|s| s.strip_suffix(']')).ok_or("expected [get_ports ...]")?;
    let w = words(s)?;
    if w.len() != 2 || w[0] != "get_ports" {
        return Err("expected [get_ports <port-list>]".into());
    }
    let ports = words_inner(&w[1])?;
    if ports.is_empty() {
        return Err("empty get_ports".into());
    }
    Ok(ports.into_iter().map(|p| unquote(&p).into()).collect())
}
fn words(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut start = None;
    let (mut braces, mut brackets, mut quote) = (0, 0, false);
    for (i, c) in s.char_indices() {
        if c == '#' && start.is_none() {
            break;
        }
        if c.is_whitespace() && braces == 0 && brackets == 0 && !quote {
            if let Some(a) = start.take() {
                out.push(s[a..i].into());
            }
            continue;
        }
        if c == ';' && braces == 0 && brackets == 0 && !quote {
            if s[i + 1..].trim().starts_with('#') || s[i + 1..].trim().is_empty() {
                if let Some(a) = start.take() {
                    out.push(s[a..i].into());
                }
                return Ok(out);
            }
            return Err("multiple commands on one line are unsupported".into());
        }
        start.get_or_insert(i);
        match c {
            '{' if !quote => braces += 1,
            '}' if !quote => {
                braces -= 1;
                if braces < 0 {
                    return Err("unmatched brace".into());
                }
            }
            '[' if braces == 0 && !quote => brackets += 1,
            ']' if braces == 0 && !quote => {
                brackets -= 1;
                if brackets < 0 {
                    return Err("unmatched bracket".into());
                }
            }
            '"' if braces == 0 => quote = !quote,
            _ => {}
        }
    }
    if braces != 0 || brackets != 0 || quote {
        return Err("unclosed Tcl grouping".into());
    }
    if let Some(a) = start {
        out.push(s[a..].into());
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn master_xdc() {
        let c=Constraints::parse("set_property -dict {PACKAGE_PIN E3 IOSTANDARD LVCMOS33} [get_ports {clk}]\nset_property PACKAGE_PIN H5 [get_ports {led[0]}]; # LED\ncreate_clock -name sys -period 10.0 [get_ports clk]").unwrap();
        assert_eq!(c.ports["clk"].pin.as_deref(), Some("E3"));
        assert_eq!(c.clocks["clk"], 10.0);
        assert!(c.ports.contains_key("led[0]"));
        assert!(Constraints::parse("create_clock -period NaN [get_ports clk]").is_err());
        assert!(Constraints::parse("exec rm foo").is_err());
    }
}
