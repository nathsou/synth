//! iCE40 configuration image and its binary bitstream encoding.
//!
//! The configuration consists of four CRAM banks (fabric configuration) and
//! four BRAM banks (block RAM initial contents). The binary format is a
//! sequence of one-byte commands with big-endian payloads, protected by a
//! CRC-16-CCITT; see Project IceStorm's format documentation. The encoder is
//! byte-for-byte compatible with `icepack`.

use std::fmt;

use crate::device::{BramMapping, CramMapping, Die, TILE_CONFIG_HEIGHT, TileKind};

/// A dense 2-D bit matrix.
#[derive(Clone, PartialEq, Eq)]
pub struct BitGrid {
    width: usize,
    height: usize,
    words: Vec<u64>,
}

impl BitGrid {
    pub fn new(width: usize, height: usize) -> Self {
        BitGrid { width, height, words: vec![0; (width * height).div_ceil(64)] }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize) -> bool {
        debug_assert!(x < self.width && y < self.height);
        let i = y * self.width + x;
        self.words[i / 64] >> (i % 64) & 1 != 0
    }

    #[inline]
    pub fn set(&mut self, x: usize, y: usize, value: bool) {
        debug_assert!(x < self.width && y < self.height);
        let i = y * self.width + x;
        if value {
            self.words[i / 64] |= 1 << (i % 64);
        } else {
            self.words[i / 64] &= !(1 << (i % 64));
        }
    }

    /// True when no bit is set.
    pub fn is_zero(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Iterates over the coordinates of the set bits in row-major order.
    pub fn ones(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let w = self.width;
        self.words.iter().enumerate().flat_map(move |(wi, &word)| {
            let mut word = word;
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                let i = wi * 64 + b;
                Some((i % w, i / w))
            })
        })
    }
}

impl fmt::Debug for BitGrid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BitGrid({}x{}, {} set)", self.width, self.height, self.ones().count())
    }
}

/// Internal oscillator frequency range used during configuration.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum FreqRange {
    #[default]
    Low,
    Medium,
    High,
}

/// A complete iCE40 configuration image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub die: Die,
    pub cram: [BitGrid; 4],
    /// Empty (zero-sized) on dies without block RAM.
    pub bram: [BitGrid; 4],
    pub freq_range: FreqRange,
    pub warmboot: bool,
    pub nosleep: bool,
    /// Raw bytes preceding the preamble (`0xFF 0x00`, comment strings, `0x00 0xFF`).
    pub header: Vec<u8>,
}

/// Errors from decoding a bitstream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid iCE40 bitstream: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

/// CRC-16-CCITT (polynomial 0x1021), MSB first, no final xor.
#[inline]
fn crc16_update(crc: u16, byte: u8) -> u16 {
    let mut crc = crc;
    for i in (0..8).rev() {
        let bit = ((crc >> 15) as u8 ^ (byte >> i)) & 1;
        crc = (crc << 1) ^ if bit != 0 { 0x1021 } else { 0 };
    }
    crc
}

struct Writer {
    out: Vec<u8>,
    crc: u16,
}

impl Writer {
    fn byte(&mut self, b: u8) {
        self.out.push(b);
        self.crc = crc16_update(self.crc, b);
    }

    fn bytes(&mut self, bs: &[u8]) {
        for &b in bs {
            self.byte(b);
        }
    }

    fn cmd16(&mut self, opcode: u8, value: usize) {
        self.bytes(&[opcode, (value >> 8) as u8, value as u8]);
    }

    /// Packs `width` x `height` bits of `grid`, starting at row `y0`, MSB first.
    fn bits(&mut self, grid: &BitGrid, width: usize, y0: usize, height: usize) {
        let mut acc = 0u8;
        let mut n = 0;
        for y in y0..y0 + height {
            for x in 0..width {
                acc = (acc << 1) | grid.get(x, y) as u8;
                n += 1;
                if n == 8 {
                    self.byte(acc);
                    acc = 0;
                    n = 0;
                }
            }
        }
        debug_assert_eq!(n, 0);
    }
}

const PREAMBLE: [u8; 4] = [0x7E, 0xAA, 0x99, 0x7E];
const BRAM_CHUNK: usize = 128;

impl Config {
    /// An all-zero configuration for `die`, with `icepack`'s defaults.
    pub fn new(die: Die) -> Self {
        let (cw, ch) = die.cram_size();
        let (bw, bh) = die.bram_size();
        Config {
            die,
            cram: std::array::from_fn(|_| BitGrid::new(cw, ch)),
            bram: std::array::from_fn(|_| BitGrid::new(bw, bh)),
            freq_range: FreqRange::Low,
            warmboot: true,
            nosleep: false,
            header: Vec::new(),
        }
    }

    /// Reads tile configuration bit `(bit_x, bit_y)` of tile `(x, y)`.
    pub fn tile_bit(&self, x: usize, y: usize, bit_x: usize, bit_y: usize) -> bool {
        let (b, cx, cy) = CramMapping::new(self.die, x, y).map(bit_x, bit_y);
        self.cram[b].get(cx, cy)
    }

    /// Writes tile configuration bit `(bit_x, bit_y)` of tile `(x, y)`.
    pub fn set_tile_bit(&mut self, x: usize, y: usize, bit_x: usize, bit_y: usize, v: bool) {
        let (b, cx, cy) = CramMapping::new(self.die, x, y).map(bit_x, bit_y);
        self.cram[b].set(cx, cy, v);
    }

    /// Reads bit `index` (0..4096) of the RAM block whose bottom tile is `(x, y)`.
    pub fn bram_bit(&self, x: usize, y: usize, index: usize) -> bool {
        let (b, bx, by) = BramMapping::new(self.die, x, y).map(index % 256, index / 256);
        self.bram[b].get(bx, by)
    }

    /// Writes bit `index` (0..4096) of the RAM block whose bottom tile is `(x, y)`.
    pub fn set_bram_bit(&mut self, x: usize, y: usize, index: usize, v: bool) {
        let (b, bx, by) = BramMapping::new(self.die, x, y).map(index % 256, index / 256);
        self.bram[b].set(bx, by, v);
    }

    /// Encodes the configuration as a binary bitstream, identical to `icepack`.
    pub fn to_bin(&self) -> Vec<u8> {
        let mut w = Writer { out: self.header.clone(), crc: 0 };
        w.bytes(&PREAMBLE);
        w.bytes(&[
            0x51,
            match self.freq_range {
                FreqRange::Low => 0,
                FreqRange::Medium => 1,
                FreqRange::High => 2,
            },
        ]);
        w.bytes(&[0x01, 0x05]);
        w.crc = 0xFFFF;
        w.bytes(&[0x92, 0x00, (if self.warmboot { 0x20 } else { 0 }) | self.nosleep as u8]);

        let (cw, ch) = self.die.cram_size();
        w.cmd16(0x62, cw - 1);
        w.cmd16(0x72, ch);
        w.cmd16(0x82, 0);
        for bank in 0..4 {
            w.bytes(&[0x11, bank as u8, 0x01, 0x01]);
            w.bits(&self.cram[bank], cw, 0, ch);
            w.bytes(&[0, 0]);
        }

        let (bw, bh) = self.die.bram_size();
        if bw != 0 && bh != 0 {
            w.cmd16(0x62, bw - 1);
            w.cmd16(0x72, BRAM_CHUNK);
            for bank in 0..4 {
                w.bytes(&[0x11, bank as u8]);
                for offset in (0..bh).step_by(BRAM_CHUNK) {
                    w.cmd16(0x82, offset);
                    w.bytes(&[0x01, 0x03]);
                    w.bits(&self.bram[bank], bw, offset, BRAM_CHUNK);
                    w.bytes(&[0, 0]);
                }
            }
        }

        w.byte(0x22);
        let crc = w.crc;
        w.bytes(&[(crc >> 8) as u8, crc as u8]);
        w.bytes(&[0x01, 0x06]);
        w.byte(0);
        w.out
    }

    /// Decodes a binary bitstream.
    pub fn from_bin(data: &[u8]) -> Result<Self, DecodeError> {
        let err = |m: String| DecodeError(m);
        let start = data
            .windows(4)
            .position(|w| w == PREAMBLE)
            .ok_or_else(|| err("no preamble found".into()))?;
        let header = data[..start].to_vec();
        let mut pos = start + 4;
        let mut crc = PREAMBLE.iter().fold(0u16, |c, &b| crc16_update(c, b));

        let next = |pos: &mut usize, crc: &mut u16| -> Result<u8, DecodeError> {
            let b = *data.get(*pos).ok_or_else(|| err("unexpected end of data".into()))?;
            *pos += 1;
            *crc = crc16_update(*crc, b);
            Ok(b)
        };

        struct Chunk {
            bram: bool,
            bank: usize,
            width: usize,
            offset: usize,
            height: usize,
            bytes: Vec<u8>,
        }
        let mut chunks = Vec::new();
        let (mut bank, mut width, mut height, mut offset) = (0usize, 0usize, 0usize, 0usize);
        let mut freq_range = FreqRange::Low;
        let (mut warmboot, mut nosleep) = (false, false);

        loop {
            let cmd = next(&mut pos, &mut crc)?;
            let mut payload = 0usize;
            for _ in 0..(cmd & 0x0F) {
                payload = (payload << 8) | next(&mut pos, &mut crc)? as usize;
            }
            match cmd & 0xF0 {
                0x00 => match payload {
                    1 | 3 => {
                        let n = width * height / 8;
                        let mut bytes = Vec::with_capacity(n);
                        for _ in 0..n {
                            bytes.push(next(&mut pos, &mut crc)?);
                        }
                        let end = [next(&mut pos, &mut crc)?, next(&mut pos, &mut crc)?];
                        if end != [0, 0] {
                            return Err(err(format!("expected 0x0000 after data, got {end:02x?}")));
                        }
                        if bank > 3 {
                            return Err(err(format!("bank {bank} out of range")));
                        }
                        chunks.push(Chunk {
                            bram: payload == 3,
                            bank,
                            width,
                            offset,
                            height,
                            bytes,
                        });
                    }
                    5 => crc = 0xFFFF,
                    6 => break,
                    _ => return Err(err(format!("unknown command 0x{cmd:02x} 0x{payload:x}"))),
                },
                0x10 => bank = payload,
                0x20 => {
                    if crc != 0 {
                        return Err(err("CRC check failed".into()));
                    }
                }
                0x50 => {
                    freq_range = match payload {
                        0 => FreqRange::Low,
                        1 => FreqRange::Medium,
                        2 => FreqRange::High,
                        _ => return Err(err(format!("unknown frequency range {payload}"))),
                    }
                }
                0x60 => width = payload + 1,
                0x70 => height = payload,
                0x80 => offset = payload,
                0x90 => match payload {
                    0 | 1 | 32 | 33 => {
                        warmboot = payload & 32 != 0;
                        nosleep = payload & 1 != 0;
                    }
                    _ => return Err(err(format!("unknown warmboot setting {payload}"))),
                },
                _ => return Err(err(format!("unknown command 0x{cmd:02x}"))),
            }
        }

        let (mut cw, mut ch) = (0, 0);
        for c in chunks.iter().filter(|c| !c.bram) {
            cw = cw.max(c.width);
            ch = ch.max(c.offset + c.height);
        }
        let die = Die::from_cram_size(cw, ch)
            .ok_or_else(|| err(format!("unknown device (CRAM {cw}x{ch})")))?;
        let mut cfg = Config::new(die);
        cfg.header = header;
        cfg.freq_range = freq_range;
        cfg.warmboot = warmboot;
        cfg.nosleep = nosleep;
        for c in &chunks {
            let grid = if c.bram { &mut cfg.bram[c.bank] } else { &mut cfg.cram[c.bank] };
            if c.width > grid.width() || c.offset + c.height > grid.height() {
                return Err(err("data chunk exceeds bank size".into()));
            }
            for (i, &byte) in c.bytes.iter().enumerate() {
                for j in 0..8 {
                    if byte << j & 0x80 != 0 {
                        let k = i * 8 + j;
                        grid.set(k % c.width, k / c.width + c.offset, true);
                    }
                }
            }
        }
        Ok(cfg)
    }

    /// Iterates over the grid positions of all non-corner tiles, row by row.
    pub fn tiles(&self) -> impl Iterator<Item = (usize, usize, TileKind)> + '_ {
        let die = self.die;
        (0..die.grid_height()).flat_map(move |y| {
            (0..die.grid_width()).filter_map(move |x| {
                let k = die.tile_kind(x, y);
                (k != TileKind::Corner).then_some((x, y, k))
            })
        })
    }

    /// CRAM bits that do not belong to any tile, as `(bank, x, y)`.
    pub fn extra_bits(&self) -> Vec<(usize, usize, usize)> {
        let (cw, ch) = self.die.cram_size();
        let mut in_tile: [BitGrid; 4] = std::array::from_fn(|_| BitGrid::new(cw, ch));
        for (x, y, kind) in self.tiles() {
            let m = CramMapping::new(self.die, x, y);
            for by in 0..TILE_CONFIG_HEIGHT {
                for bx in 0..kind.config_width() {
                    let (b, cx, cy) = m.map(bx, by);
                    in_tile[b].set(cx, cy, true);
                }
            }
        }
        let mut out = Vec::new();
        for (b, grid) in in_tile.iter().enumerate() {
            for x in 0..cw {
                for y in 0..ch {
                    if self.cram[b].get(x, y) && !grid.get(x, y) {
                        out.push((b, x, y));
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_value() {
        // CRC-16/CCITT-FALSE of "123456789" is 0x29B1.
        let crc = b"123456789".iter().fold(0xFFFF, |c, &b| crc16_update(c, b));
        assert_eq!(crc, 0x29B1);
    }

    #[test]
    fn bin_roundtrip() {
        for die in Die::ALL {
            let mut cfg = Config::new(die);
            cfg.header = vec![0xFF, 0x00, b'h', b'i', 0x00, 0x00, 0xFF];
            for (i, (x, y, kind)) in cfg.tiles().collect::<Vec<_>>().into_iter().enumerate() {
                cfg.set_tile_bit(x, y, i % kind.config_width(), i % 16, true);
                if kind == TileKind::RamB {
                    cfg.set_bram_bit(x, y, (i * 37) % 4096, true);
                }
            }
            let bin = cfg.to_bin();
            let back = Config::from_bin(&bin).unwrap();
            assert_eq!(back, cfg, "{die}");
            // A flipped data bit must be caught by the CRC.
            let mut bad = bin.clone();
            let mid = bad.len() / 2;
            bad[mid] ^= 0x10;
            assert!(Config::from_bin(&bad).is_err());
        }
    }
}
