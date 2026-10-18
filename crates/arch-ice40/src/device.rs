//! iCE40 device geometry: tile grid, tile types and configuration memory sizes.
//!
//! The figures follow Project IceStorm's documentation of the configuration
//! memory layout (`icepack`).

use std::fmt;
use std::str::FromStr;

/// An iCE40 die. Packages and speed grades share a die's fabric.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Die {
    /// LP384.
    D384,
    /// LP1K / HX1K.
    D1k,
    /// LP8K / HX8K (also LP4K / HX4K, which are 8K dies).
    D8k,
}

/// The kind of a tile, which decides the width of its configuration block.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TileKind {
    Corner,
    Io,
    Logic,
    /// Bottom half of a RAM block (holds most RAM configuration).
    RamB,
    /// Top half of a RAM block.
    RamT,
}

impl TileKind {
    /// Width in bits of the tile's configuration block (the height is always 16).
    pub const fn config_width(self) -> usize {
        match self {
            TileKind::Corner => 0,
            TileKind::Io => 18,
            TileKind::Logic => 54,
            TileKind::RamB | TileKind::RamT => 42,
        }
    }

    /// The name used in `.asc` files and the IceStorm chip database.
    pub const fn name(self) -> &'static str {
        match self {
            TileKind::Corner => "corner",
            TileKind::Io => "io",
            TileKind::Logic => "logic",
            TileKind::RamB => "ramb",
            TileKind::RamT => "ramt",
        }
    }

    pub const ALL: [TileKind; 5] =
        [TileKind::Corner, TileKind::Io, TileKind::Logic, TileKind::RamB, TileKind::RamT];
}

/// Height in bits of every tile's configuration block.
pub const TILE_CONFIG_HEIGHT: usize = 16;

impl Die {
    pub const ALL: [Die; 3] = [Die::D384, Die::D1k, Die::D8k];

    /// The name used by IceStorm (`384`, `1k`, `8k`).
    pub const fn name(self) -> &'static str {
        match self {
            Die::D384 => "384",
            Die::D1k => "1k",
            Die::D8k => "8k",
        }
    }

    /// Number of logic/RAM tile columns (the full grid adds one IO column per side).
    pub const fn width(self) -> usize {
        match self {
            Die::D384 => 6,
            Die::D1k => 12,
            Die::D8k => 32,
        }
    }

    /// Number of logic/RAM tile rows (the full grid adds one IO row per side).
    pub const fn height(self) -> usize {
        match self {
            Die::D384 => 8,
            Die::D1k => 16,
            Die::D8k => 32,
        }
    }

    /// Width of the full tile grid including the IO ring.
    pub const fn grid_width(self) -> usize {
        self.width() + 2
    }

    /// Height of the full tile grid including the IO ring.
    pub const fn grid_height(self) -> usize {
        self.height() + 2
    }

    /// Configuration block widths of the columns of one CRAM bank, starting
    /// at the bank's outer edge.
    pub const fn bank_columns(self) -> &'static [usize] {
        match self {
            Die::D384 => &[18, 54, 54, 54, 54],
            Die::D1k => &[18, 54, 54, 42, 54, 54, 54],
            Die::D8k => &[18, 54, 54, 54, 54, 54, 54, 54, 42, 54, 54, 54, 54, 54, 54, 54, 54],
        }
    }

    /// The kind of tile at grid position `(x, y)`.
    pub fn tile_kind(self, x: usize, y: usize) -> TileKind {
        let (w, h) = (self.width(), self.height());
        let edge_x = x == 0 || x == w + 1;
        let edge_y = y == 0 || y == h + 1;
        if edge_x && edge_y {
            return TileKind::Corner;
        }
        if edge_x || edge_y {
            return TileKind::Io;
        }
        let ram_cols: &[usize] = match self {
            Die::D384 => &[],
            Die::D1k => &[3, 10],
            Die::D8k => &[8, 25],
        };
        if ram_cols.contains(&x) {
            if y % 2 == 1 { TileKind::RamB } else { TileKind::RamT }
        } else {
            TileKind::Logic
        }
    }

    /// CRAM bank dimensions `(width, height)`.
    pub const fn cram_size(self) -> (usize, usize) {
        match self {
            Die::D384 => (182, 80),
            Die::D1k => (332, 144),
            Die::D8k => (872, 272),
        }
    }

    /// BRAM bank dimensions `(width, height)`; zero when the die has no RAM.
    pub const fn bram_size(self) -> (usize, usize) {
        match self {
            Die::D384 => (0, 0),
            Die::D1k => (64, 256),
            Die::D8k => (128, 256),
        }
    }

    /// Identifies a die from its CRAM bank size.
    pub fn from_cram_size(width: usize, height: usize) -> Option<Die> {
        Die::ALL.into_iter().find(|d| d.cram_size() == (width, height))
    }
}

impl fmt::Display for Die {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Die {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Die::ALL
            .into_iter()
            .find(|d| d.name() == s)
            .ok_or_else(|| format!("unsupported iCE40 device `{s}` (supported: 384, 1k, 8k)"))
    }
}

/// Maps tile-relative configuration bits to CRAM bank coordinates.
///
/// Each of the four CRAM banks holds one quadrant of the chip, addressed from
/// the chip corner outwards; top and bottom IO tiles additionally permute their
/// bits.
#[derive(Copy, Clone, Debug)]
pub struct CramMapping {
    kind: TileKind,
    left_right_io: bool,
    right_half: bool,
    top_half: bool,
    bank: usize,
    xoff: usize,
    yoff: usize,
    column_width: usize,
}

const IO_TOP_BOTTOM_PERM_X: [usize; 18] =
    [23, 25, 26, 27, 16, 17, 18, 19, 20, 14, 32, 33, 34, 35, 36, 37, 4, 5];
const IO_TOP_BOTTOM_PERM_Y: [usize; 16] = [0, 1, 3, 2, 4, 5, 7, 6, 8, 9, 11, 10, 12, 13, 15, 14];

impl CramMapping {
    pub fn new(die: Die, x: usize, y: usize) -> Self {
        let (w, h) = (die.width(), die.height());
        let kind = die.tile_kind(x, y);
        let right_half = x > w / 2;
        let top_half = y > h / 2;
        let mut bank = 0;
        if top_half {
            bank |= 1;
        }
        if right_half {
            bank |= 2;
        }
        let bank_tx = if right_half { w + 1 - x } else { x };
        let bank_ty = if top_half { h + 1 - y } else { y };
        let cols = die.bank_columns();
        CramMapping {
            kind,
            left_right_io: x == 0 || x == w + 1,
            right_half,
            top_half,
            bank,
            xoff: cols[..bank_tx].iter().sum(),
            yoff: 16 * bank_ty,
            column_width: cols[bank_tx],
        }
    }

    pub fn kind(&self) -> TileKind {
        self.kind
    }

    /// Returns `(bank, cram_x, cram_y)` for tile bit `(bit_x, bit_y)`, where
    /// `bit_y` is the row (0..16) and `bit_x` the column within the row.
    #[inline]
    pub fn map(&self, bit_x: usize, bit_y: usize) -> (usize, usize, usize) {
        let (cx, cy);
        if self.kind == TileKind::Io && !self.left_right_io {
            cy = self.yoff + 15 - IO_TOP_BOTTOM_PERM_Y[bit_y];
            cx = if self.right_half {
                self.xoff + self.column_width - 1 - IO_TOP_BOTTOM_PERM_X[bit_x]
            } else {
                self.xoff + IO_TOP_BOTTOM_PERM_X[bit_x]
            };
        } else if self.kind == TileKind::Io {
            cx = self.xoff + self.column_width - 1 - bit_x;
            cy = if self.top_half { self.yoff + 15 - bit_y } else { self.yoff + bit_y };
        } else {
            cx = if self.right_half {
                self.xoff + self.column_width - 1 - bit_x
            } else {
                self.xoff + bit_x
            };
            cy = if self.top_half { self.yoff + 15 - bit_y } else { self.yoff + bit_y };
        }
        (self.bank, cx, cy)
    }
}

/// Maps bits of a RAM tile's 4 Kbit initial contents to BRAM bank coordinates.
#[derive(Copy, Clone, Debug)]
pub struct BramMapping {
    bank: usize,
    offset: usize,
}

impl BramMapping {
    /// `(x, y)` is the position of the RAM block's bottom (`ramb`) tile.
    pub fn new(die: Die, x: usize, y: usize) -> Self {
        let (w, h) = (die.width(), die.height());
        let right_half = x > w / 2;
        let top_half = y > h / 2;
        let mut bank = 0;
        let mut y_offset = y - 1;
        if top_half {
            bank |= 1;
            y_offset = y - h / 2;
        }
        if right_half {
            bank |= 2;
        }
        BramMapping { bank, offset: 16 * (y_offset / 2) }
    }

    /// Returns `(bank, bram_x, bram_y)` for bit `bit_x` (0..256) of row
    /// `bit_y` (0..16) of the RAM contents.
    #[inline]
    pub fn map(&self, bit_x: usize, bit_y: usize) -> (usize, usize, usize) {
        let index = 256 * bit_y + (16 * (bit_x / 16) + 15 - bit_x % 16);
        (self.bank, self.offset + index % 16, index / 16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_and_kinds() {
        let d = Die::D1k;
        assert_eq!(d.tile_kind(0, 0), TileKind::Corner);
        assert_eq!(d.tile_kind(0, 5), TileKind::Io);
        assert_eq!(d.tile_kind(3, 1), TileKind::RamB);
        assert_eq!(d.tile_kind(3, 2), TileKind::RamT);
        assert_eq!(d.tile_kind(1, 1), TileKind::Logic);
        assert_eq!(Die::from_cram_size(872, 272), Some(Die::D8k));
        assert_eq!("8k".parse::<Die>(), Ok(Die::D8k));
    }

    /// Every tile bit must map to a distinct CRAM bit inside the bank.
    #[test]
    fn cram_mapping_is_injective() {
        for die in Die::ALL {
            let (cw, ch) = die.cram_size();
            let mut seen = vec![false; 4 * cw * ch];
            for y in 0..die.grid_height() {
                for x in 0..die.grid_width() {
                    let m = CramMapping::new(die, x, y);
                    for by in 0..TILE_CONFIG_HEIGHT {
                        for bx in 0..m.kind().config_width() {
                            let (b, cx, cy) = m.map(bx, by);
                            assert!(cx < cw && cy < ch, "{die} ({x},{y}) bit ({bx},{by})");
                            let i = (b * ch + cy) * cw + cx;
                            assert!(!seen[i], "{die}: collision at ({x},{y}) bit ({bx},{by})");
                            seen[i] = true;
                        }
                    }
                }
            }
        }
    }
}
