//! The geometry of the global clock network (`globals.json`).
//!
//! A device has 16 global clocks per quadrant. A quadrant's global
//! (`G_ULPCLK<n>`, driven by the center multiplexers) reaches its spines
//! along a row (`G_HPRX<n>00` in a spine tile), each spine runs up and down
//! the quadrant in a tap column (`G_VPTX<n>00`), each tap drives the rows'
//! branches to its left and right (`L_HPBX<n>00`, `R_HPBX<n>00`), and the
//! branch reaches every tile of its segment (`G_HPBX<n>00`). Those three
//! links are wires, not switches, so the tile databases do not have them.

use synth_core::Json;

#[derive(Clone, Debug)]
struct Quadrant {
    name: String,
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
}

#[derive(Clone, Debug)]
struct Tap {
    col: usize,
    left: (usize, usize),
    right: (usize, usize),
}

#[derive(Clone, Debug)]
struct Spine {
    quadrant: String,
    tap_col: usize,
    row: usize,
    col: usize,
}

/// Which side of its tap a tile's branch is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Debug, Default)]
pub struct Geometry {
    quadrants: Vec<Quadrant>,
    taps: Vec<Tap>,
    spines: Vec<Spine>,
}

fn uint(j: &Json, key: &str) -> Option<usize> {
    j.get(key).and_then(Json::as_i64).map(|v| v as usize)
}

impl Geometry {
    pub fn from_json(j: &Json) -> Geometry {
        let mut g = Geometry::default();
        if let Json::Obj(qs) = j.at(&["quadrants"]) {
            for (name, q) in qs {
                if let (Some(x0), Some(x1), Some(y0), Some(y1)) =
                    (uint(q, "x0"), uint(q, "x1"), uint(q, "y0"), uint(q, "y1"))
                {
                    g.quadrants.push(Quadrant { name: name.clone(), x0, x1, y0, y1 });
                }
            }
        }
        if let Json::Obj(ts) = j.at(&["taps"]) {
            for (name, t) in ts {
                let col = name.strip_prefix('C').and_then(|c| c.parse().ok());
                if let (Some(col), Some(lx0), Some(lx1), Some(rx0), Some(rx1)) =
                    (col, uint(t, "lx0"), uint(t, "lx1"), uint(t, "rx0"), uint(t, "rx1"))
                {
                    g.taps.push(Tap { col, left: (lx0, lx1), right: (rx0, rx1) });
                }
            }
        }
        if let Json::Obj(ss) = j.at(&["spines"]) {
            for (name, s) in ss {
                if name.len() < 3 {
                    continue;
                }
                let (quadrant, tap) = name.split_at(2);
                if let (Ok(tap_col), Some(col), Some(row)) =
                    (tap.parse(), uint(s, "x"), uint(s, "y"))
                {
                    g.spines.push(Spine { quadrant: quadrant.to_string(), tap_col, row, col });
                }
            }
        }
        g
    }

    /// The quadrant (`UL`, `UR`, `LL`, `LR`) of a tile.
    pub fn quadrant(&self, row: usize, col: usize) -> Option<&str> {
        self.quadrants
            .iter()
            .find(|q| (q.x0..=q.x1).contains(&col) && (q.y0..=q.y1).contains(&row))
            .map(|q| q.name.as_str())
    }

    /// The tap column driving a tile's branch, and which side it is on.
    pub fn tap(&self, col: usize) -> Option<(Side, usize)> {
        self.taps.iter().find_map(|t| {
            if (t.left.0..=t.left.1).contains(&col) {
                Some((Side::Left, t.col))
            } else if (t.right.0..=t.right.1).contains(&col) {
                Some((Side::Right, t.col))
            } else {
                None
            }
        })
    }

    /// The spine tile (row, column) of a quadrant's tap column.
    pub fn spine(&self, quadrant: &str, tap_col: usize) -> Option<(usize, usize)> {
        self.spines
            .iter()
            .find(|s| s.quadrant == quadrant && s.tap_col == tap_col)
            .map(|s| (s.row, s.col))
    }

    /// The spines of a quadrant (row, column).
    pub fn spines_of<'a>(&'a self, quadrant: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.spines.iter().filter(move |s| s.quadrant == quadrant).map(|s| (s.row, s.col))
    }
}
