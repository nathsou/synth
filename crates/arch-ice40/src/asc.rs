//! The IceStorm `.asc` text format: one 16-row block of `0`/`1` characters per
//! tile, hex block RAM contents, and stray CRAM bits as `.extra_bit` lines.
//!
//! Reading and writing match `icepack` and `iceunpack`, so `.asc` files from
//! other tools convert to bit-identical bitstreams.

use std::fmt::Write as _;

use crate::bitstream::Config;
use crate::device::{CramMapping, Die, TILE_CONFIG_HEIGHT, TileKind};

/// An error while parsing an `.asc` file, with its 1-based line number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AscError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for AscError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for AscError {}

fn parse_usize(tok: Option<&str>, line: usize, what: &str) -> Result<usize, AscError> {
    tok.and_then(|t| t.parse().ok())
        .ok_or_else(|| AscError { line, message: format!("expected {what}") })
}

/// Parses an `.asc` file.
pub fn read_asc(text: &str) -> Result<Config, AscError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut cfg: Option<Config> = None;
    let mut header: Option<Vec<u8>> = None;
    let mut warmboot = true;
    let mut i = 0;

    let need_cfg = |cfg: &mut Option<Config>, line: usize| -> Result<(), AscError> {
        if cfg.is_none() {
            return Err(AscError { line, message: "missing .device statement".into() });
        }
        Ok(())
    };

    while i < lines.len() {
        let line_no = i + 1;
        let line = lines[i];
        i += 1;
        let mut toks = line.split_whitespace();
        let Some(cmd) = toks.next() else { continue };
        match cmd {
            ".comment" => {
                let mut h = vec![0xFF, 0x00];
                while i < lines.len() && !lines[i].starts_with('.') {
                    h.extend_from_slice(lines[i].as_bytes());
                    h.push(0);
                    i += 1;
                }
                h.extend_from_slice(&[0x00, 0xFF]);
                header = Some(h);
            }
            ".device" => {
                if cfg.is_some() {
                    return Err(AscError { line: line_no, message: "duplicate .device".into() });
                }
                let name = toks.next().unwrap_or("");
                let die: Die = name.parse().map_err(|m| AscError { line: line_no, message: m })?;
                cfg = Some(Config::new(die));
            }
            ".warmboot" => {
                warmboot = match toks.next() {
                    Some("enabled") => true,
                    Some("disabled") => false,
                    other => {
                        return Err(AscError {
                            line: line_no,
                            message: format!("unknown warmboot setting {other:?}"),
                        });
                    }
                }
            }
            ".io_tile" | ".logic_tile" | ".ramb_tile" | ".ramt_tile" => {
                need_cfg(&mut cfg, line_no)?;
                let c = cfg.as_mut().unwrap();
                let x = parse_usize(toks.next(), line_no, "tile x")?;
                let y = parse_usize(toks.next(), line_no, "tile y")?;
                if x >= c.die.grid_width() || y >= c.die.grid_height() {
                    return Err(AscError { line: line_no, message: "tile out of range".into() });
                }
                let m = CramMapping::new(c.die, x, y);
                if format!(".{}_tile", m.kind().name()) != cmd {
                    return Err(AscError {
                        line: line_no,
                        message: format!("{cmd} given for {} tile {x} {y}", m.kind().name()),
                    });
                }
                for by in 0..TILE_CONFIG_HEIGHT {
                    if i >= lines.len() || lines[i].starts_with('.') {
                        break;
                    }
                    for (bx, ch) in lines[i].bytes().take(m.kind().config_width()).enumerate() {
                        if ch == b'1' {
                            let (b, cx, cy) = m.map(bx, by);
                            c.cram[b].set(cx, cy, true);
                        }
                    }
                    i += 1;
                }
            }
            ".ram_data" => {
                need_cfg(&mut cfg, line_no)?;
                let c = cfg.as_mut().unwrap();
                let x = parse_usize(toks.next(), line_no, "tile x")?;
                let y = parse_usize(toks.next(), line_no, "tile y")?;
                if c.die.tile_kind(x, y) != TileKind::RamB {
                    return Err(AscError { line: line_no, message: "not a RAM tile".into() });
                }
                for row in 0..16 {
                    if i >= lines.len() || lines[i].starts_with('.') {
                        break;
                    }
                    // Each line holds 256 bits as 64 hex digits, most significant first.
                    for (k, ch) in lines[i].trim_end().chars().take(64).enumerate() {
                        let v = ch.to_digit(16).ok_or_else(|| AscError {
                            line: i + 1,
                            message: format!("not a hex digit: {ch:?}"),
                        })?;
                        let bit_x = 256 - 4 - 4 * k;
                        for b in 0..4 {
                            if v >> b & 1 != 0 {
                                c.set_bram_bit(x, y, row * 256 + bit_x + b, true);
                            }
                        }
                    }
                    i += 1;
                }
            }
            ".extra_bit" => {
                need_cfg(&mut cfg, line_no)?;
                let c = cfg.as_mut().unwrap();
                let b = parse_usize(toks.next(), line_no, "bank")?;
                let x = parse_usize(toks.next(), line_no, "x")?;
                let y = parse_usize(toks.next(), line_no, "y")?;
                let (cw, ch) = c.die.cram_size();
                if b >= 4 || x >= cw || y >= ch {
                    return Err(AscError {
                        line: line_no,
                        message: "extra bit out of range".into(),
                    });
                }
                c.cram[b].set(x, y, true);
            }
            ".sym" => {}
            _ if cmd.starts_with('.') => {
                return Err(AscError {
                    line: line_no,
                    message: format!("unknown statement {cmd}"),
                });
            }
            _ => {
                return Err(AscError {
                    line: line_no,
                    message: format!("unexpected line {line:?}"),
                });
            }
        }
    }

    let mut cfg = cfg.ok_or(AscError { line: 0, message: "missing .device statement".into() })?;
    cfg.warmboot = warmboot;
    if let Some(h) = header {
        cfg.header = h;
    }
    Ok(cfg)
}

/// Writes a configuration in `.asc` format, matching `iceunpack`.
pub fn write_asc(cfg: &Config) -> String {
    let mut out = String::with_capacity(1 << 20);
    out.push_str(".comment");
    let mut newline = true;
    for &ch in &cfg.header {
        match ch {
            0 => newline = true,
            0xFF => newline = false,
            _ => {
                if newline {
                    out.push('\n');
                }
                out.push(ch as char);
                newline = false;
            }
        }
    }
    let _ = writeln!(out, "\n.device {}", cfg.die);
    if !cfg.warmboot {
        out.push_str(".warmboot disabled\n");
    }
    for (x, y, kind) in cfg.tiles() {
        let _ = writeln!(out, ".{}_tile {x} {y}", kind.name());
        let m = CramMapping::new(cfg.die, x, y);
        for by in 0..TILE_CONFIG_HEIGHT {
            for bx in 0..kind.config_width() {
                let (b, cx, cy) = m.map(bx, by);
                out.push(if cfg.cram[b].get(cx, cy) { '1' } else { '0' });
            }
            out.push('\n');
        }
        if kind == TileKind::RamB {
            let _ = writeln!(out, ".ram_data {x} {y}");
            for row in 0..16 {
                for k in 0..64 {
                    let bit_x = 256 - 4 - 4 * k;
                    let mut v = 0;
                    for b in 0..4 {
                        if cfg.bram_bit(x, y, row * 256 + bit_x + b) {
                            v |= 1 << b;
                        }
                    }
                    out.push(char::from_digit(v, 16).unwrap());
                }
                out.push('\n');
            }
        }
    }
    for (b, x, y) in cfg.extra_bits() {
        let _ = writeln!(out, ".extra_bit {b} {x} {y}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_roundtrip() {
        let mut cfg = Config::new(Die::D1k);
        cfg.set_tile_bit(1, 1, 36, 0, true);
        cfg.set_tile_bit(0, 5, 17, 3, true);
        cfg.set_bram_bit(3, 1, 4095, true);
        cfg.set_bram_bit(3, 1, 0, true);
        cfg.cram[0].set(330, 142, true); // not in any tile
        cfg.warmboot = false;
        let text = write_asc(&cfg);
        let back = read_asc(&text).unwrap();
        assert_eq!(back.cram, cfg.cram);
        assert_eq!(back.bram, cfg.bram);
        assert!(!back.warmboot);
        assert_eq!(write_asc(&back), text);
    }
}
