//! A tile type's configuration bits (Project Trellis `bits.db`): routing
//! multiplexers, multi-bit words and enumerated settings, each a group of
//! bits of the tile's window in the configuration memory, and the fixed
//! connections of the tile's wires.
//!
//! Writing a setting sets its bits (an inverted bit, `!F1B2`, is set by
//! clearing it); reading a tile back takes, for each multiplexer and
//! setting, the matching option with the most bits, as Trellis does, so
//! that tiles read back exactly as `ecpunpack` reads them.

use std::collections::BTreeMap;

use synth_core::FxHashSet;

use crate::config::TileConfig;

/// The configuration memory: `frames` frames of `bits` bits each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cram {
    pub frames: usize,
    pub bits: usize,
    data: Vec<u8>,
}

impl Cram {
    pub fn new(frames: usize, bits: usize) -> Cram {
        Cram { frames, bits, data: vec![0; frames * bits] }
    }

    #[inline]
    pub fn get(&self, frame: usize, bit: usize) -> bool {
        self.data[frame * self.bits + bit] != 0
    }

    #[inline]
    pub fn set(&mut self, frame: usize, bit: usize, value: bool) {
        self.data[frame * self.bits + bit] = value as u8;
    }
}

/// A tile's window into the configuration memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub frame: usize,
    pub bit: usize,
    pub frames: usize,
    pub bits: usize,
}

impl Window {
    #[inline]
    fn get(&self, cram: &Cram, b: ConfigBit) -> bool {
        cram.get(self.frame + b.frame as usize, self.bit + b.bit as usize)
    }

    #[inline]
    fn set(&self, cram: &mut Cram, b: ConfigBit, value: bool) {
        cram.set(self.frame + b.frame as usize, self.bit + b.bit as usize, value)
    }
}

/// One bit of a tile: frame and bit within the tile's window, and whether
/// the setting is active when the bit is clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfigBit {
    pub frame: u16,
    pub bit: u16,
    pub inv: bool,
}

impl ConfigBit {
    fn parse(s: &str) -> Result<ConfigBit, String> {
        let (inv, rest) = match s.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, s),
        };
        let bad = || format!("bad configuration bit `{s}`");
        let (f, b) = rest.strip_prefix('F').and_then(|r| r.split_once('B')).ok_or_else(bad)?;
        Ok(ConfigBit {
            frame: f.parse().map_err(|_| bad())?,
            bit: b.parse().map_err(|_| bad())?,
            inv,
        })
    }
}

impl std::fmt::Display for ConfigBit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}F{}B{}", if self.inv { "!" } else { "" }, self.frame, self.bit)
    }
}

/// Bits that are all set (or, inverted, clear) together.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitGroup(pub Vec<ConfigBit>);

impl BitGroup {
    fn parse<'a>(tokens: impl Iterator<Item = &'a str>) -> Result<BitGroup, String> {
        let mut bits = Vec::new();
        for t in tokens {
            if t == "-" {
                break;
            }
            bits.push(ConfigBit::parse(t)?);
        }
        bits.sort_unstable();
        bits.dedup();
        Ok(BitGroup(bits))
    }

    /// Whether every bit has its active value.
    pub fn matches(&self, cram: &Cram, w: &Window) -> bool {
        self.0.iter().all(|&b| w.get(cram, b) != b.inv)
    }

    /// How many bits do not have their active value.
    pub fn mismatches(&self, cram: &Cram, w: &Window) -> usize {
        self.0.iter().filter(|&&b| w.get(cram, b) == b.inv).count()
    }

    fn set(&self, cram: &mut Cram, w: &Window) {
        for &b in &self.0 {
            w.set(cram, b, !b.inv);
        }
    }

    fn clear(&self, cram: &mut Cram, w: &Window) {
        for &b in &self.0 {
            w.set(cram, b, b.inv);
        }
    }

    /// Records the bits that hold `value` (set, unless inverted) as known.
    fn cover(&self, known: &mut FxHashSet<(u16, u16)>, value: bool) {
        for b in &self.0 {
            if b.inv != value {
                known.insert((b.frame, b.bit));
            }
        }
    }
}

/// A routing multiplexer: its sink wire and the bits selecting each source.
#[derive(Clone, Debug, Default)]
pub struct Mux {
    pub sink: String,
    /// By source name (in Trellis' order, which decides ties when reading).
    pub arcs: BTreeMap<String, BitGroup>,
}

/// A multi-bit setting, least significant bit first.
#[derive(Clone, Debug, Default)]
pub struct Word {
    pub name: String,
    pub default: Vec<bool>,
    pub bits: Vec<BitGroup>,
}

/// A setting taking one of several named values.
#[derive(Clone, Debug, Default)]
pub struct Enum {
    pub name: String,
    pub default: Option<String>,
    pub options: BTreeMap<String, BitGroup>,
}

/// A wire always driven by another.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FixedConn {
    pub sink: String,
    pub source: String,
}

/// The configuration bits of a tile type.
#[derive(Clone, Debug, Default)]
pub struct TileBits {
    pub muxes: BTreeMap<String, Mux>,
    pub words: BTreeMap<String, Word>,
    pub enums: BTreeMap<String, Enum>,
    pub fixed: Vec<FixedConn>,
}

/// Parses a word's value, written most significant bit first.
pub fn parse_word(s: &str) -> Result<Vec<bool>, String> {
    s.chars()
        .rev()
        .map(|c| match c {
            '0' => Ok(false),
            '1' => Ok(true),
            _ => Err(format!("bad word value `{s}`")),
        })
        .collect()
}

/// A word's value, most significant bit first.
pub fn word_string(v: &[bool]) -> String {
    v.iter().rev().map(|&b| if b { '1' } else { '0' }).collect()
}

impl TileBits {
    /// Parses a `bits.db` file.
    pub fn parse(text: &str) -> Result<TileBits, String> {
        let mut db = TileBits::default();
        // Records start at a line whose first token begins with '.'.
        let mut lines = text
            .lines()
            .map(|l| l.split('#').next().unwrap_or("").trim())
            .filter(|l| !l.is_empty())
            .peekable();
        while let Some(line) = lines.next() {
            let mut head = line.split_whitespace();
            let kind = head.next().unwrap_or("");
            let mut body = Vec::new();
            while let Some(l) = lines.peek() {
                if l.starts_with('.') {
                    break;
                }
                body.push(lines.next().unwrap());
            }
            let name =
                || head.clone().next().map(str::to_string).ok_or(format!("`{line}`: no name"));
            match kind {
                ".mux" => {
                    let sink = name()?;
                    let mut arcs = BTreeMap::new();
                    for l in body {
                        let mut t = l.split_whitespace();
                        let source = t.next().unwrap_or("").to_string();
                        arcs.insert(source, BitGroup::parse(t)?);
                    }
                    db.muxes.insert(sink.clone(), Mux { sink, arcs });
                }
                ".config" => {
                    let name = name()?;
                    let bits = body
                        .iter()
                        .map(|l| BitGroup::parse(l.split_whitespace()))
                        .collect::<Result<Vec<_>, _>>()?;
                    let default = match head.nth(1) {
                        Some(d) => parse_word(d)?,
                        None => vec![false; bits.len()],
                    };
                    db.words.insert(name.clone(), Word { name, default, bits });
                }
                ".config_enum" => {
                    let name = name()?;
                    let default = head.nth(1).map(str::to_string);
                    let mut options = BTreeMap::new();
                    for l in body {
                        let mut t = l.split_whitespace();
                        let opt = t.next().unwrap_or("").to_string();
                        options.insert(opt, BitGroup::parse(t)?);
                    }
                    db.enums.insert(name.clone(), Enum { name, default, options });
                }
                ".fixed_conn" => {
                    let mut t = line.split_whitespace().skip(1);
                    let (Some(sink), Some(source)) = (t.next(), t.next()) else {
                        return Err(format!("bad fixed connection `{line}`"));
                    };
                    db.fixed.push(FixedConn { sink: sink.into(), source: source.into() });
                }
                _ => return Err(format!("unexpected `{kind}` in a tile database")),
            }
        }
        db.fixed.sort();
        db.fixed.dedup();
        Ok(db)
    }

    /// Writes `cfg` into the tile's window. With `group` (settings shared
    /// by a group of tiles), names the tile does not have are skipped and
    /// those found are added to `matched`; otherwise unknown names are
    /// errors and every setting not given takes its default.
    pub fn apply(
        &self,
        cfg: &TileConfig,
        cram: &mut Cram,
        w: &Window,
        group: Option<&mut FxHashSet<String>>,
    ) -> Result<(), String> {
        let is_group = group.is_some();
        let mut matched = group;
        for (sink, source) in &cfg.arcs {
            let mux = self.muxes.get(sink).ok_or_else(|| format!("no routing mux for `{sink}`"))?;
            let bits = mux
                .arcs
                .get(source)
                .ok_or_else(|| format!("sink {sink} has no driver {source}"))?;
            bits.set(cram, w);
        }
        let mut found_words = FxHashSet::default();
        let mut found_enums = FxHashSet::default();
        // Base settings (such as an IO type) first, so that others overlay
        // them.
        let mut set_enum = |name: &str,
                            value: &str,
                            cram: &mut Cram,
                            matched: &mut Option<&mut FxHashSet<String>>|
         -> Result<(), String> {
            let e = match self.enums.get(name) {
                Some(e) => e,
                None if is_group => return Ok(()),
                None => return Err(format!("no setting named `{name}`")),
            };
            if is_group && !e.options.contains_key(value) {
                return Ok(());
            }
            if let Some(m) = matched {
                m.insert(name.to_string());
            }
            if value != "_NONE_" {
                let bits = e
                    .options
                    .get(value)
                    .ok_or_else(|| format!("setting `{name}` has no value `{value}`"))?;
                bits.set(cram, w);
            }
            found_enums.insert(name.to_string());
            Ok(())
        };
        for (name, value) in cfg.enums.iter().filter(|(n, _)| n.starts_with("BASE_")) {
            set_enum(name, value, cram, &mut matched)?;
        }
        for (name, value) in &cfg.words {
            let word = match self.words.get(name) {
                Some(word) => word,
                None if is_group => continue,
                None => return Err(format!("no word named `{name}`")),
            };
            if let Some(m) = &mut matched {
                m.insert(name.clone());
            }
            if value.len() != word.bits.len() {
                return Err(format!(
                    "word `{name}` has {} bits, not {}",
                    word.bits.len(),
                    value.len()
                ));
            }
            for (bits, &v) in word.bits.iter().zip(value) {
                if v { bits.set(cram, w) } else { bits.clear(cram, w) }
            }
            found_words.insert(name.clone());
        }
        for (name, value) in cfg.enums.iter().filter(|(n, _)| !n.starts_with("BASE_")) {
            set_enum(name, value, cram, &mut matched)?;
        }
        for &(f, b) in &cfg.unknowns {
            cram.set(w.frame + f as usize, w.bit + b as usize, true);
        }
        if !is_group {
            for (name, word) in &self.words {
                if !found_words.contains(name) {
                    for (bits, &v) in word.bits.iter().zip(&word.default) {
                        if v { bits.set(cram, w) } else { bits.clear(cram, w) }
                    }
                }
            }
            for (name, e) in &self.enums {
                if let Some(d) = &e.default
                    && !found_enums.contains(name)
                    && let Some(bits) = e.options.get(d)
                {
                    bits.set(cram, w);
                }
            }
        }
        Ok(())
    }

    /// Reads the tile's settings back from its window.
    pub fn read(&self, cram: &Cram, w: &Window) -> TileConfig {
        let mut cfg = TileConfig::default();
        let mut known = FxHashSet::default();
        // The matching option with the most bits (the last such on a tie).
        fn best<'a>(
            options: impl Iterator<Item = (&'a String, &'a BitGroup)>,
            cram: &Cram,
            w: &Window,
        ) -> Option<(&'a String, &'a BitGroup)> {
            let mut found = None;
            let mut most = 0;
            for (name, bits) in options {
                if bits.matches(cram, w) && bits.0.len() >= most {
                    most = bits.0.len();
                    found = Some((name, bits));
                }
            }
            found
        }
        for (sink, mux) in &self.muxes {
            if let Some((source, bits)) = best(mux.arcs.iter(), cram, w) {
                bits.cover(&mut known, true);
                if !bits.0.is_empty() {
                    cfg.arcs.push((sink.clone(), source.clone()));
                }
            }
        }
        for (name, word) in &self.words {
            let value: Vec<bool> = word
                .bits
                .iter()
                .map(|bits| {
                    let m = bits.matches(cram, w);
                    bits.cover(&mut known, m);
                    m
                })
                .collect();
            if value != word.default {
                cfg.words.push((name.clone(), value));
            }
        }
        for (name, e) in &self.enums {
            match best(e.options.iter(), cram, w) {
                None => {
                    if e.default.is_some() {
                        cfg.enums.push((name.clone(), "_NONE_".into()));
                    }
                }
                Some((value, bits)) => {
                    bits.cover(&mut known, true);
                    let is_default =
                        e.default.as_ref().is_some_and(|d| e.options.get(d) == Some(bits));
                    if !is_default {
                        cfg.enums.push((name.clone(), value.clone()));
                    }
                }
            }
        }
        for f in 0..w.frames {
            for b in 0..w.bits {
                if cram.get(w.frame + f, w.bit + b) && !known.contains(&(f as u16, b as u16)) {
                    cfg.unknowns.push((f as u16, b as u16));
                }
            }
        }
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DB: &str = "\
# Routing Mux Bits
.mux A0
E1_H01E0001 F1B2 F6B3
F5 F1B3 F4B2
V02N0701 -

# Non-Routing Configuration
.config SLICEA.K0.INIT 1111
!F25B10
!F24B10
!F23B10
!F22B10

.config_enum CLK0.CLKMUX CLK
CLK !F54B10
INV F54B10

.config_enum SLICEA.A0MUX A0
1 F2B3 F4B2
A0 -

# Fixed Connections
.fixed_conn A0_SLICE A0
";

    #[test]
    fn settings_read_back_as_written() {
        let db = TileBits::parse(DB).unwrap();
        assert_eq!(db.muxes["A0"].arcs.len(), 3);
        assert_eq!(db.words["SLICEA.K0.INIT"].default, vec![true; 4]);
        assert_eq!(db.fixed, vec![FixedConn { sink: "A0_SLICE".into(), source: "A0".into() }]);
        let w = Window { frame: 2, bit: 1, frames: 60, bits: 12 };
        // An empty configuration is all defaults: nothing reads back.
        let mut cram = Cram::new(64, 16);
        db.apply(&TileConfig::default(), &mut cram, &w, None).unwrap();
        assert_eq!(db.read(&cram, &w), TileConfig::default());
        let cfg = TileConfig {
            arcs: vec![("A0".into(), "F5".into())],
            words: vec![("SLICEA.K0.INIT".into(), parse_word("0110").unwrap())],
            enums: vec![("CLK0.CLKMUX".into(), "INV".into())],
            unknowns: vec![(40, 5)],
        };
        let mut cram = Cram::new(64, 16);
        db.apply(&cfg, &mut cram, &w, None).unwrap();
        assert!(cram.get(2 + 1, 1 + 3) && cram.get(2 + 4, 1 + 2));
        // Inverted word bits: value bit 0 (0) sets its bit, bit 1 (1) clears it.
        assert!(cram.get(2 + 25, 1 + 10) && !cram.get(2 + 24, 1 + 10));
        assert_eq!(db.read(&cram, &w), cfg);
        // An enum with no matching option reads back as `_NONE_`.
        let mut cram = Cram::new(64, 16);
        db.apply(&cfg, &mut cram, &w, None).unwrap();
        db.enums["SLICEA.A0MUX"].options["1"].set(&mut cram, &w);
        let read = db.read(&cram, &w);
        assert!(read.enums.contains(&("SLICEA.A0MUX".into(), "1".into())));
    }
}
