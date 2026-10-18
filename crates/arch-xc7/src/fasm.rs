//! Feature-to-frame assembly using X-Ray segbits, including aliases, negated
//! bits and conflict detection. FASM is a debug/interchange format, not a runtime
//! dependency on an external assembler.
use crate::bitstream::{self, Frames};
use crate::device::Device;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
struct Segbit {
    frame: u32,
    bit: u32,
    set: bool,
}
#[derive(Debug)]
pub struct Assembler<'a> {
    device: &'a Device,
    root: PathBuf,
    tables: BTreeMap<(String, String), BTreeMap<String, Vec<Segbit>>>,
    ppips: BTreeMap<String, BTreeSet<String>>,
}
fn canonical(feature: &str) -> Result<String, String> {
    if let Some((prefix, index)) = feature.strip_suffix(']').and_then(|s| s.rsplit_once('[')) {
        let index: u32 = index.parse().map_err(|_| format!("invalid feature index: {feature}"))?;
        Ok(format!("{prefix}[{index}]"))
    } else if feature.contains(['[', ']']) {
        Err(format!("invalid feature {feature}"))
    } else {
        Ok(feature.into())
    }
}
impl<'a> Assembler<'a> {
    pub fn new(device: &'a Device, root: &Path) -> Self {
        Self { device, root: root.into(), tables: BTreeMap::new(), ppips: BTreeMap::new() }
    }
    fn table(&mut self, kind: &str, bus: &str) -> Result<&BTreeMap<String, Vec<Segbit>>, String> {
        let key = (kind.to_string(), bus.to_string());
        if !self.tables.contains_key(&key) {
            let suffix = match bus {
                "CLB_IO_CLK" => "",
                "BLOCK_RAM" => ".block_ram",
                _ => return Err(format!("unsupported feature bus {bus}")),
            };
            let path = self.root.join(format!("segbits_{}{suffix}.db", kind.to_lowercase()));
            let mut table = BTreeMap::new();
            match std::fs::read_to_string(&path) {
                Ok(s) => {
                    for (line_no, line) in s.lines().enumerate() {
                        let mut tokens = line.split_whitespace();
                        let Some(feature) = tokens.next() else { continue };
                        let (_, feature) =
                            feature.split_once('.').ok_or("feature has no tile type")?;
                        let mut bits = Vec::new();
                        for token in tokens {
                            let (set, token) = if let Some(t) = token.strip_prefix('!') {
                                (false, t)
                            } else {
                                (true, token)
                            };
                            let (frame, bit) = token.split_once('_').ok_or_else(|| {
                                format!(
                                    "{}:{}: unresolved segbit {token}",
                                    path.display(),
                                    line_no + 1
                                )
                            })?;
                            bits.push(Segbit {
                                frame: frame.parse().map_err(|_| "invalid segbit frame")?,
                                bit: bit.parse().map_err(|_| "invalid segbit bit")?,
                                set,
                            });
                        }
                        if table.insert(canonical(feature)?, bits).is_some() {
                            return Err(format!("duplicate feature {feature}"));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
            self.tables.insert(key.clone(), table);
        }
        Ok(&self.tables[&key])
    }
    fn is_ppip(&mut self, kind: &str, feature: &str) -> Result<bool, String> {
        if !self.ppips.contains_key(kind) {
            let path = self.root.join(format!("ppips_{}.db", kind.to_lowercase()));
            let mut set = BTreeSet::new();
            match std::fs::read_to_string(&path) {
                Ok(s) => {
                    for line in s.lines() {
                        if let Some((_, f)) =
                            line.split_whitespace().next().and_then(|s| s.split_once('.'))
                        {
                            set.insert(f.into());
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
            self.ppips.insert(kind.into(), set);
        }
        Ok(self.ppips[kind].contains(feature))
    }
    /// Assemble a set of enabled single-bit features. Explicit zero feature
    /// values are omitted by the parser; negated segbits still constrain zeros.
    pub fn assemble(&mut self, features: &BTreeSet<String>) -> Result<Frames, String> {
        let mut frames = bitstream::blank(&self.device.part);
        let mut assigned: BTreeMap<(u32, u32), (bool, String)> = BTreeMap::new();
        for full in features {
            let (tile, feature) =
                full.split_once('.').ok_or_else(|| format!("feature has no tile: {full}"))?;
            let &tile_id =
                self.device.tile_by_name.get(tile).ok_or_else(|| format!("unknown tile {tile}"))?;
            let tile = &self.device.tiles[tile_id];
            if self.is_ppip(&tile.kind, feature)? {
                continue;
            }
            let mut found = false;
            for (bus, region) in &tile.bits {
                let (kind, offset, mapped) = if let Some((kind, start, sites)) = &region.alias {
                    let mut parts: Vec<_> = feature.split('.').collect();
                    if let Some(site) = sites.get(parts[0]) {
                        parts[0] = site;
                    }
                    (kind.as_str(), i64::from(region.offset) - i64::from(*start), parts.join("."))
                } else {
                    (tile.kind.as_str(), i64::from(region.offset), feature.to_string())
                };
                let Some(bits) = self.table(kind, bus)?.get(&canonical(&mapped)?) else { continue };
                found = true;
                for b in bits {
                    let word = offset + i64::from(b.bit / 32);
                    if b.frame >= region.frames
                        || !(i64::from(region.offset)
                            ..i64::from(region.offset) + i64::from(region.words))
                            .contains(&word)
                    {
                        return Err(format!("feature {full} exceeds its frame extent"));
                    }
                    let address = region.base + b.frame;
                    let bit = word as u32 * 32 + b.bit % 32;
                    if word < i64::from(region.offset)
                        || word >= i64::from(region.offset + region.words)
                    {
                        return Err(format!("feature {full} exceeds alias extent"));
                    }
                    if word == 50 && bit % 32 < 13 {
                        return Err(format!("feature {full} overlaps ECC"));
                    }
                    let frame = frames.get_mut(&address).ok_or_else(|| {
                        format!("feature {full} addresses frame {address:08x} outside part")
                    })?;
                    if let Some((old, other)) = assigned.get(&(address, bit)) {
                        if *old != b.set {
                            return Err(format!(
                                "configuration conflict: {full} and {other} at {address:08x}:{bit}"
                            ));
                        }
                    } else {
                        assigned.insert((address, bit), (b.set, full.clone()));
                    }
                    if b.set {
                        frame[word as usize] |= 1 << (bit % 32);
                    } else {
                        frame[word as usize] &= !(1 << (bit % 32));
                    }
                }
            }
            if !found {
                return Err(format!("unknown configuration feature {full}"));
            }
        }
        Ok(frames)
    }
}

/// Parse scalar features and binary/hex/decimal vector assignments. Annotations
/// are deliberately rejected, rather than silently changing their meaning.
pub fn parse(source: &str) -> Result<BTreeSet<String>, String> {
    let mut result = BTreeSet::new();
    for (line_no, line) in source.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let add = |e: String| format!("FASM line {}: {e}", line_no + 1);
        let (name, value) = match line.split_once('=') {
            Some((n, v)) => (n.trim(), Some(v.trim())),
            None => (line, None),
        };
        if name.is_empty()
            || name.bytes().any(|c| !c.is_ascii_alphanumeric() && !b"_.[]:".contains(&c))
        {
            return Err(add("invalid feature name".into()));
        }
        let (base, lo, hi, indexed) =
            if let Some((base, range)) = name.strip_suffix(']').and_then(|s| s.rsplit_once('[')) {
                let (hi, lo) = range.split_once(':').unwrap_or((range, range));
                let hi: usize = hi.parse().map_err(|_| add("invalid range".into()))?;
                let lo: usize = lo.parse().map_err(|_| add("invalid range".into()))?;
                if hi < lo || hi - lo > 1_000_000 {
                    return Err(add("invalid range size".into()));
                }
                (base, lo, hi, true)
            } else {
                (name, 0, 0, false)
            };
        let mut bits = vec![false; hi - lo + 1];
        if let Some(value) = value {
            if let Some((width, digits)) = value.split_once('\'') {
                let width: usize = width.parse().map_err(|_| add("invalid value width".into()))?;
                if width != bits.len() {
                    return Err(add("value width does not match feature range".into()));
                }
                let radix = match digits.as_bytes().first() {
                    Some(b'b' | b'B') => 2,
                    Some(b'h' | b'H') => 16,
                    Some(b'd' | b'D') => 10,
                    _ => return Err(add("unsupported value radix".into())),
                };
                parse_number(&digits[1..], radix, &mut bits).map_err(add)?;
            } else {
                parse_number(value, 10, &mut bits).map_err(add)?;
            }
        } else {
            if hi != lo {
                return Err(add("vector feature needs a value".into()));
            }
            bits[0] = true;
        }
        for (i, on) in bits.into_iter().enumerate() {
            if on {
                result.insert(if indexed { format!("{base}[{}]", lo + i) } else { base.into() });
            }
        }
    }
    Ok(result)
}
fn parse_number(s: &str, radix: u32, bits: &mut [bool]) -> Result<(), String> {
    let mut any = false;
    for c in s.chars().filter(|&c| c != '_') {
        let mut carry = c.to_digit(radix).ok_or("invalid feature value")?;
        any = true;
        for b in bits.iter_mut() {
            let v = u32::from(*b) * radix + carry;
            *b = v & 1 != 0;
            carry = v >> 1;
        }
        if carry != 0 {
            return Err("feature value overflows range".into());
        }
    }
    if !any {
        return Err("empty feature value".into());
    }
    Ok(())
}
pub fn text(features: &BTreeSet<String>) -> String {
    features.iter().map(|f| format!("{f}\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vectors() {
        assert_eq!(
            parse("T.L.INIT[7:0] = 8'h81\nT.A=0\nT.B\n").unwrap(),
            BTreeSet::from(["T.L.INIT[0]".into(), "T.L.INIT[7]".into(), "T.B".into()])
        );
        for bad in ["T.X[2:0]=3'hf", "T.X[2:1]", "T.X=2", "T.X[-1]", "T.X[0:7]=8'b0"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
