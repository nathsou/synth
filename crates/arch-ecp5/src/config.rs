//! A chip's configuration as named settings: the textual configuration of
//! Project Trellis (`ecppack` reads it, `ecpunpack` writes it).
//!
//! ```text
//! .device LFE5U-25F
//! .tile R2C3:PLC2
//! arc: A0 H02E0701
//! word: SLICEA.K0.INIT 1000000000000000
//! enum: SLICEA.MODE CCU2
//! .bram_init 5
//! 000 001 ...
//! .tile_group R50C10:MIB_EBR0 R50C11:MIB_EBR1
//! enum: EBR.MODE DP16KD
//! ```

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::bits::{parse_word, word_string};

/// The settings of one tile (or of a group of tiles).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TileConfig {
    /// Routing switches as (sink, source) wire names.
    pub arcs: Vec<(String, String)>,
    /// Words, least significant bit first.
    pub words: Vec<(String, Vec<bool>)>,
    pub enums: Vec<(String, String)>,
    /// Set bits no setting explains, as (frame, bit) in the tile.
    pub unknowns: Vec<(u16, u16)>,
}

impl TileConfig {
    pub fn is_empty(&self) -> bool {
        self.arcs.is_empty()
            && self.words.is_empty()
            && self.enums.is_empty()
            && self.unknowns.is_empty()
    }

    pub fn add_arc(&mut self, sink: &str, source: &str) {
        self.arcs.push((sink.into(), source.into()));
    }

    pub fn add_word(&mut self, name: &str, value: Vec<bool>) {
        self.words.push((name.into(), value));
    }

    pub fn add_enum(&mut self, name: &str, value: &str) {
        self.enums.push((name.into(), value.into()));
    }

    fn write(&self, out: &mut String) {
        for (sink, source) in &self.arcs {
            let _ = writeln!(out, "arc: {sink} {source}");
        }
        for (name, value) in &self.words {
            let _ = writeln!(out, "word: {name} {}", word_string(value));
        }
        for (name, value) in &self.enums {
            let _ = writeln!(out, "enum: {name} {value}");
        }
        for (f, b) in &self.unknowns {
            let _ = writeln!(out, "unknown: F{f}B{b}");
        }
    }
}

/// Settings shared by a group of tiles (a block RAM or a clock buffer
/// spans several), each applied to the tiles that have it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TileGroup {
    pub tiles: Vec<String>,
    pub config: TileConfig,
}

/// A whole chip's configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChipConfig {
    pub device: String,
    pub variant: String,
    pub comments: Vec<String>,
    pub sysconfig: BTreeMap<String, String>,
    /// By full tile name (`R2C3:PLC2`).
    pub tiles: BTreeMap<String, TileConfig>,
    /// Initial contents of block RAMs, by index: 2048 words of 9 bits.
    pub bram_init: BTreeMap<u16, Vec<u16>>,
    pub groups: Vec<TileGroup>,
}

impl ChipConfig {
    pub fn tile(&mut self, name: &str) -> &mut TileConfig {
        self.tiles.entry(name.to_string()).or_default()
    }

    /// Parses the textual form.
    pub fn parse(text: &str) -> Result<ChipConfig, String> {
        let mut cc = ChipConfig::default();
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty()).peekable();
        // The entries of a record, up to the next line starting with '.'.
        fn entries<'a>(
            lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
        ) -> Vec<&'a str> {
            let mut body = Vec::new();
            while let Some(l) = lines.peek() {
                if l.starts_with('.') {
                    break;
                }
                body.push(lines.next().unwrap());
            }
            body
        }
        fn tile_config(body: &[&str]) -> Result<TileConfig, String> {
            let mut tc = TileConfig::default();
            for l in body {
                let t: Vec<&str> = l.split_whitespace().collect();
                match t.as_slice() {
                    ["arc:", sink, source] => tc.add_arc(sink, source),
                    ["word:", name, value] => tc.add_word(name, parse_word(value)?),
                    ["enum:", name, value] => tc.add_enum(name, value),
                    ["unknown:", bit] => {
                        let bad = || format!("bad unknown bit `{bit}`");
                        let (f, b) = bit
                            .strip_prefix('F')
                            .and_then(|r| r.split_once('B'))
                            .ok_or_else(bad)?;
                        tc.unknowns
                            .push((f.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?));
                    }
                    _ => return Err(format!("unexpected `{l}` in a tile configuration")),
                }
            }
            Ok(tc)
        }
        while let Some(line) = lines.next() {
            let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim();
            match verb {
                ".device" => cc.device = rest.to_string(),
                ".variant" => cc.variant = rest.to_string(),
                ".comment" => cc.comments.push(rest.to_string()),
                ".sysconfig" => {
                    let (k, v) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                    cc.sysconfig.insert(k.to_string(), v.trim().to_string());
                }
                ".tile" => {
                    let body = entries(&mut lines);
                    cc.tiles.insert(rest.to_string(), tile_config(&body)?);
                }
                ".bram_init" => {
                    let index: u16 = rest.parse().map_err(|_| format!("bad block RAM `{rest}`"))?;
                    let mut data = Vec::new();
                    for l in entries(&mut lines) {
                        for w in l.split_whitespace() {
                            data.push(
                                u16::from_str_radix(w, 16)
                                    .map_err(|_| format!("bad block RAM word `{w}`"))?,
                            );
                        }
                    }
                    cc.bram_init.insert(index, data);
                }
                ".tile_group" => {
                    let tiles = rest.split_whitespace().map(str::to_string).collect();
                    let body = entries(&mut lines);
                    cc.groups.push(TileGroup { tiles, config: tile_config(&body)? });
                }
                _ => return Err(format!("unexpected `{verb}` in a chip configuration")),
            }
        }
        Ok(cc)
    }

    /// The textual form, in Trellis' order.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, ".device {}\n", self.device);
        if !self.variant.is_empty() {
            let _ = writeln!(out, ".variant {}\n", self.variant);
        }
        for c in &self.comments {
            let _ = writeln!(out, ".comment {c}");
        }
        for (k, v) in &self.sysconfig {
            let _ = writeln!(out, ".sysconfig {k} {v}");
        }
        out.push('\n');
        for (name, tc) in &self.tiles {
            if !tc.is_empty() {
                let _ = writeln!(out, ".tile {name}");
                tc.write(&mut out);
                out.push('\n');
            }
        }
        for (index, data) in &self.bram_init {
            let _ = writeln!(out, ".bram_init {index}");
            for (i, w) in data.iter().enumerate() {
                let _ = write!(out, "{w:03x}{}", if i % 8 == 7 { "\n" } else { " " });
            }
            out.push('\n');
        }
        for g in &self.groups {
            let _ = writeln!(out, ".tile_group {}", g.tiles.join(" "));
            g.config.write(&mut out);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips() {
        let text = "\
.device LFE5U-25F

.comment Part: LFE5U-25F-6CABGA381
.sysconfig COMPRESS_CONFIG ON

.tile R2C3:PLC2
arc: A0 H02E0701
word: SLICEA.K0.INIT 1000000000000001
enum: SLICEA.MODE CCU2
unknown: F3B4

.bram_init 5
000 001 002 003 004 005 006 1ff

.tile_group R50C10:MIB_EBR0 R50C11:MIB_EBR1
enum: EBR.MODE DP16KD

";
        let cc = ChipConfig::parse(text).unwrap();
        assert!(cc.tiles["R2C3:PLC2"].words[0].1[0]);
        assert_eq!(cc.bram_init[&5][7], 0x1ff);
        assert_eq!(cc.groups[0].tiles.len(), 2);
        assert_eq!(cc.to_text(), text);
    }
}
