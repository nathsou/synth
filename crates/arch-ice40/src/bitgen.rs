//! Configuration (bitstream) generation for a placed and routed design.

use synth_netlist::{CellId, Netlist, Param};

use crate::arch::{Arch, BelKind};
use crate::bitstream::Config;
use crate::chipdb::{ChipDb, TileBit};
use crate::device::{Die, TileKind};
use crate::pack::{GB, IO, LC, RAM};
use crate::place::Placement;
use crate::route::Routing;

/// LUT truth table bit `i` goes to LC configuration bit `LUT_PERM[i]`.
const LUT_PERM: [usize; 16] = [4, 14, 15, 5, 6, 16, 17, 7, 3, 13, 12, 2, 1, 11, 10, 0];

struct Writer<'a> {
    db: &'a ChipDb,
    cfg: Config,
    errors: Vec<String>,
}

impl Writer<'_> {
    fn bit(&mut self, x: usize, y: usize, b: TileBit, value: bool) {
        let cur = self.cfg.tile_bit(x, y, b.col as usize, b.row as usize);
        if cur && !value {
            self.errors
                .push(format!("configuration conflict at tile ({x},{y}) B{}[{}]", b.row, b.col));
        }
        self.cfg.set_tile_bit(x, y, b.col as usize, b.row as usize, value);
    }

    /// Sets configuration entry `name` of tile `(x, y)` (all its bits, or
    /// just bit `index`).
    fn entry(&mut self, x: usize, y: usize, name: &str, value: bool, index: Option<usize>) {
        let Some(t) = self.db.template_at(x, y) else {
            self.errors.push(format!("no tile at ({x},{y})"));
            return;
        };
        let Some(e) = t.config(name) else {
            self.errors.push(format!("tile ({x},{y}) has no configuration entry {name}"));
            return;
        };
        let bits: Vec<TileBit> = match index {
            Some(i) => e.bits.get(i).copied().into_iter().collect(),
            None => e.bits.clone(),
        };
        for b in bits {
            self.bit(x, y, b, value);
        }
    }

    fn has_entry(&self, x: usize, y: usize, name: &str) -> bool {
        self.db.template_at(x, y).is_some_and(|t| t.config(name).is_some())
    }
}

fn int_param(nl: &Netlist, c: CellId, name: &str) -> u64 {
    nl.param(c, name).and_then(Param::as_u64).unwrap_or(0)
}

/// Generates the configuration image.
pub fn generate(
    arch: &Arch,
    nl: &Netlist,
    placement: &Placement,
    routing: &Routing,
) -> Result<Config, String> {
    let db = arch.db;
    let die = db.die;
    let is_1k = die == Die::D1k;
    let mut w = Writer { db, cfg: Config::new(die), errors: Vec::new() };

    // Routing.
    for edges in routing.net_edges.values() {
        for &e in edges {
            let pip = arch.graph.edge_pip[e as usize];
            let (x, y, t, tp) = arch.pip_info(pip);
            let sw = &t.switches[tp.switch as usize];
            for (k, &b) in sw.bits.iter().enumerate() {
                let v = tp.pattern >> k & 1 != 0;
                if v {
                    w.bit(x, y, b, true);
                }
            }
        }
    }

    let mut used_ios = std::collections::HashSet::new();
    let mut used_rams = std::collections::HashSet::new();
    for c in nl.live_cells() {
        let Some(&bel_id) = placement.cell_bel.get(&c) else {
            if nl.cell_kind(c) == IO && placement.cell_global.contains_key(&c) {
                continue;
            }
            return Err(format!("cell {} is not placed", nl.str(nl.cells[c].name)));
        };
        let bel = arch.bel(bel_id);
        let (x, y, z) = (bel.x as usize, bel.y as usize, bel.z as usize);
        match nl.cell_kind(c) {
            LC => {
                let init = int_param(nl, c, "LUT_INIT") as u16;
                let mut lc = [false; 20];
                for (i, &p) in LUT_PERM.iter().enumerate() {
                    lc[p] = init >> i & 1 != 0;
                }
                let dff = int_param(nl, c, "DFF_ENABLE") != 0;
                lc[8] = int_param(nl, c, "CARRY_ENABLE") != 0;
                lc[9] = dff;
                lc[18] = int_param(nl, c, "SET_NORESET") != 0;
                lc[19] = int_param(nl, c, "ASYNC_SR") != 0;
                let name = format!("LC_{z}");
                for (i, &v) in lc.iter().enumerate() {
                    if v {
                        w.entry(x, y, &name, true, Some(i));
                    }
                }
                if dff && int_param(nl, c, "NEG_CLK") != 0 {
                    w.entry(x, y, "NegClk", true, None);
                }
                if int_param(nl, c, "CIN_CONST") != 0 && int_param(nl, c, "CIN_SET") != 0 {
                    if z != 0 {
                        w.errors.push("constant-1 carry input not at slot 0".into());
                    }
                    w.entry(x, y, "CarryInSet", true, None);
                }
            }
            IO => {
                used_ios.insert(bel_id);
                let pin_type = int_param(nl, c, "PIN_TYPE");
                for i in 0..6 {
                    if pin_type >> i & 1 != 0 {
                        w.entry(x, y, &format!("IOB_{z}.PINTYPE_{i}"), true, None);
                    }
                }
                if (nl.pin_net(c, "INPUT_CLK").is_some() || nl.pin_net(c, "OUTPUT_CLK").is_some())
                    && int_param(nl, c, "NEG_TRIGGER") != 0
                {
                    w.entry(x, y, "NegClk", true, None);
                }
                let input_en = nl.pin_net(c, "D_IN_0").is_some()
                    || nl.pin_net(c, "D_IN_1").is_some()
                    || placement.cell_global.contains_key(&c);
                let pullup = int_param(nl, c, "PULLUP") != 0;
                if let Some(ie) = db.ieren.iter().find(|e| e.io == (x as u8, y as u8, z as u8)) {
                    let (ix, iy, iz) = (ie.ieren.0 as usize, ie.ieren.1 as usize, ie.ieren.2);
                    w.entry(
                        ix,
                        iy,
                        &format!("IoCtrl.IE_{iz}"),
                        if is_1k { !input_en } else { input_en },
                        None,
                    );
                    w.entry(ix, iy, &format!("IoCtrl.REN_{iz}"), !pullup, None);
                }
                if let Some(&g) = placement.cell_global.get(&c) {
                    let (bank, ex, ey) = db.globals[g as usize].pad_extra_bit;
                    w.cfg.cram[bank as usize].set(ex as usize, ey as usize, true);
                }
            }
            RAM => {
                used_rams.insert(bel_id);
                if !is_1k {
                    w.entry(x, y, "RamConfig.PowerUp", true, None);
                }
                let (nr, nw) =
                    (int_param(nl, c, "NEG_CLK_R") != 0, int_param(nl, c, "NEG_CLK_W") != 0);
                let (bottom, top) = if is_1k { (nw, nr) } else { (nr, nw) };
                if bottom {
                    w.entry(x, y, "NegClk", true, None);
                }
                if top {
                    w.entry(x, y + 1, "NegClk", true, None);
                }
                let wm = int_param(nl, c, "WRITE_MODE");
                let rm = int_param(nl, c, "READ_MODE");
                for (i, v) in [wm & 1, wm & 2, rm & 1, rm & 2].into_iter().enumerate() {
                    if v != 0 {
                        w.entry(x, y + 1, &format!("RamConfig.CBIT_{i}"), true, None);
                    }
                }
                for word in 0..16 {
                    let name = format!("INIT_{word:X}");
                    let Some(p) = nl.param(c, &name) else { continue };
                    for (i, b) in p.to_bits(256).iter().enumerate() {
                        if b.is_one() {
                            w.cfg.set_bram_bit(x, y, word * 256 + i, true);
                        }
                    }
                }
            }
            GB => {}
            other => return Err(format!("cannot configure cell type {other}")),
        }
    }

    // Unused IOs: input buffers per family default, pull-ups enabled.
    for b in arch.bels_of(BelKind::Io) {
        if used_ios.contains(&b) {
            continue;
        }
        let bel = arch.bel(b);
        if let Some(ie) = db.ieren.iter().find(|e| e.io == (bel.x, bel.y, bel.z)) {
            let (ix, iy, iz) = (ie.ieren.0 as usize, ie.ieren.1 as usize, ie.ieren.2);
            w.entry(ix, iy, &format!("IoCtrl.IE_{iz}"), is_1k, None);
        }
    }
    // Unused RAMs on 1K dies keep their power-up bit set.
    if is_1k {
        for b in arch.bels_of(BelKind::Ram) {
            if !used_rams.contains(&b) {
                let bel = arch.bel(b);
                w.entry(bel.x as usize, bel.y as usize, "RamConfig.PowerUp", true, None);
            }
        }
    }

    // Column buffers: enable all global networks in the buffer rows.
    for y in 0..die.grid_height() {
        for x in 0..die.grid_width() {
            let kind = die.tile_kind(x, y);
            if kind == TileKind::Corner {
                continue;
            }
            let ram = matches!(kind, TileKind::RamB | TileKind::RamT);
            let rows: &[usize] = match die {
                Die::D384 => &[],
                Die::D1k if ram => &[3, 5, 11, 13],
                Die::D1k => &[4, 5, 12, 13],
                Die::D8k => &[8, 9, 24, 25],
            };
            if rows.contains(&y) {
                for g in 0..8 {
                    let name = format!("ColBufCtrl.glb_netwk_{g}");
                    if w.has_entry(x, y, &name) {
                        w.entry(x, y, &name, true, None);
                    }
                }
            }
        }
    }

    if !w.errors.is_empty() {
        let n = w.errors.len();
        w.errors.truncate(10);
        return Err(format!("bitstream generation failed ({n} errors): {}", w.errors.join("; ")));
    }
    Ok(w.cfg)
}
