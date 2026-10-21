//! A configuration image back to a netlist of packed cells (the reverse of
//! [`crate::bitgen`]), to simulate what a bitstream implements.
//!
//! Every active switch joins its two routing nodes; the logic cells, block
//! RAMs and global buffers that are configured become packed cells
//! cells on the joined nodes, with their parameters read back from their
//! bits (`ICESTORM_LC`, `ICESTORM_RAM`, `SB_GB`). Simple input and output pads named in the pin constraints become
//! the netlist's ports. Unconnected inputs are left unconnected (their
//! default value applies). Registered or tristate pads and PLLs are not
//! decoded.

use synth_core::FxHashMap;
use synth_netlist::{Bit, CellId, Dir, INPUT_PORT, NetId, Netlist, OUTPUT_PORT, Param};

use crate::arch::{Arch, BelKind};
use crate::bitstream::Config;
use crate::chipdb::NodeId;
use crate::device::Die;
use crate::pack::{GB, LC, RAM};
use crate::place::PinConstraints;

/// LUT truth table bit `i` is LC configuration bit `LUT_PERM[i]`.
const LUT_PERM: [usize; 16] = [4, 14, 15, 5, 6, 16, 17, 7, 3, 13, 12, 2, 1, 11, 10, 0];

struct Uf(Vec<u32>);

impl Uf {
    fn find(&mut self, mut a: u32) -> u32 {
        while self.0[a as usize] != a {
            let p = self.0[a as usize];
            self.0[a as usize] = self.0[p as usize];
            a = p;
        }
        a
    }

    fn union(&mut self, a: u32, b: u32) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[a.max(b) as usize] = a.min(b);
        }
    }
}

struct Decoder<'a> {
    arch: &'a Arch,
    cfg: &'a Config,
    uf: Uf,
    nl: Netlist,
    nets: FxHashMap<u32, NetId>,
    /// Joined nodes with a driver.
    driven: Vec<bool>,
    warnings: Vec<String>,
}

impl Decoder<'_> {
    /// Whether all bits of configuration entry `name` of tile (x, y) are set
    /// (bit `index` only, if given).
    fn entry(&self, x: usize, y: usize, name: &str, index: Option<usize>) -> bool {
        let Some(t) = self.arch.db.template_at(x, y) else { return false };
        let Some(e) = t.config(name) else { return false };
        let bits: Vec<_> = match index {
            Some(i) => e.bits.get(i).copied().into_iter().collect(),
            None => e.bits.clone(),
        };
        !bits.is_empty()
            && bits.iter().all(|b| self.cfg.tile_bit(x, y, b.col as usize, b.row as usize))
    }

    fn net(&mut self, node: NodeId) -> NetId {
        let r = self.uf.find(node.raw());
        if let Some(&n) = self.nets.get(&r) {
            return n;
        }
        let name = self.arch.db.node_name(NodeId::new(r as usize));
        let n = self.nl.add_net(&name);
        self.nets.insert(r, n);
        n
    }

    fn is_driven(&mut self, node: NodeId) -> bool {
        let r = self.uf.find(node.raw());
        self.driven[r as usize]
    }

    fn pin(&mut self, c: CellId, name: &str, dir: Dir, node: Option<NodeId>) -> Result<(), String> {
        let net = match (dir, node) {
            (Dir::Output, Some(n)) => Some(self.net(n)),
            (_, Some(n)) if self.is_driven(n) => Some(self.net(n)),
            _ => None,
        };
        self.nl.add_pin(c, name, dir, net).map(|_| ()).map_err(|e| e.0)
    }
}

/// Decodes `cfg` for `arch`, naming the ports after `pins` (port → package
/// pin). Returns the netlist and warnings about what was not decoded.
pub fn decode(
    cfg: &Config,
    arch: &Arch,
    pins: &PinConstraints,
) -> Result<(Netlist, Vec<String>), String> {
    let db = arch.db;
    let n = db.nodes.len();
    let mut uf = Uf((0..n as u32).collect());
    // Routing: join both sides of every active switch.
    let gw = db.grid_width();
    for y in 0..db.grid_height() {
        for x in 0..gw {
            let Some(t) = db.template_at(x, y) else { continue };
            for p in &t.pips {
                if p.pattern == 0 {
                    continue;
                }
                let sw = &t.switches[p.switch as usize];
                let on = sw.bits.iter().enumerate().all(|(k, b)| {
                    cfg.tile_bit(x, y, b.col as usize, b.row as usize) == (p.pattern >> k & 1 != 0)
                });
                if !on {
                    continue;
                }
                if let (Some(a), Some(b)) =
                    (db.tile_wire_node(x, y, p.src), db.tile_wire_node(x, y, sw.dst))
                {
                    uf.union(a.raw(), b.raw());
                }
            }
        }
    }
    let mut d = Decoder {
        arch,
        cfg,
        uf,
        nl: Netlist::new(),
        nets: FxHashMap::default(),
        driven: vec![false; n],
        warnings: Vec::new(),
    };
    d.nl.top = Some(d.nl.sym("top"));
    let is_1k = db.die == Die::D1k;

    // Which cells are configured, and what drives each node.
    struct LcInfo {
        bel: usize,
        bits: [bool; 20],
    }
    let mut size = vec![0u32; n];
    for i in 0..n as u32 {
        let r = d.uf.find(i);
        size[r as usize] += 1;
    }
    let mut lcs = Vec::new();
    let mut rams = Vec::new();
    for (bi, bel) in arch.bels.iter().enumerate() {
        let (x, y, z) = (bel.x as usize, bel.y as usize, bel.z as usize);
        match bel.kind {
            BelKind::Lc => {
                let name = format!("LC_{z}");
                let bits: [bool; 20] = std::array::from_fn(|i| d.entry(x, y, &name, Some(i)));
                if bits.iter().any(|&b| b) {
                    lcs.push(LcInfo { bel: bi, bits });
                }
            }
            BelKind::Ram => {
                // In use when a clock reaches it through some switch.
                let clocked = bel
                    .pins
                    .iter()
                    .filter(|p| matches!(p.name, "RCLK" | "WCLK"))
                    .any(|p| size[d.uf.find(p.node.raw()) as usize] > 1);
                if clocked {
                    rams.push(bi);
                }
            }
            _ => {}
        }
    }
    let mark = |d: &mut Decoder, node: NodeId| {
        let r = d.uf.find(node.raw());
        d.driven[r as usize] = true;
    };
    for lc in &lcs {
        let bel = &arch.bels[lc.bel];
        for p in &bel.pins {
            if p.name == "O" || p.name == "COUT" && lc.bits[8] {
                mark(&mut d, p.node);
            }
        }
    }
    for &bi in &rams {
        for p in &arch.bels[bi].pins {
            if p.name.starts_with("RDATA") {
                mark(&mut d, p.node);
            }
        }
    }
    // Pads: package pin of each constrained port.
    let mut ports = Vec::new();
    for (port, pin) in pins.iter() {
        let Some(pp) = arch.package.pins.iter().find(|p| &p.name == pin) else {
            d.warnings.push(format!("port {port}: pin {pin} is not in the package"));
            continue;
        };
        let (x, y, z) = (pp.x as usize, pp.y as usize, pp.z as usize);
        let Some(bi) = arch.bel_at(pp.x, pp.y, pp.z, BelKind::Io) else { continue };
        let pin_type: u64 = (0..6)
            .filter(|&i| d.entry(x, y, &format!("IOB_{z}.PINTYPE_{i}"), None))
            .map(|i| 1 << i)
            .sum();
        ports.push((port.clone(), bi.index(), pin_type, (x, y, z)));
        if pin_type & 3 == 1 {
            let node = arch.bels[bi.index()].pins.iter().find(|p| p.name == "D_IN_0").unwrap().node;
            mark(&mut d, node);
        }
    }
    // Global networks driven from the fabric or a pad.
    let mut globals = Vec::new();
    for (g, glb) in db.globals.iter().enumerate() {
        let (bank, ex, ey) = glb.pad_extra_bit;
        let from_pad = cfg.cram[bank as usize].get(ex as usize, ey as usize);
        let Some(out) = db.node_by_name(
            glb.fabout.0 as usize,
            glb.fabout.1 as usize,
            &format!("glb_netwk_{g}"),
        ) else {
            continue;
        };
        let src = if from_pad {
            let (x, y, z) = glb.pad;
            let Some(bi) = arch.bel_at(x, y, z, BelKind::Io) else { continue };
            arch.bels[bi.index()].pins.iter().find(|p| p.name == "D_IN_0").map(|p| p.node)
        } else {
            db.node_by_name(glb.fabout.0 as usize, glb.fabout.1 as usize, "fabout")
        };
        if let Some(src) = src {
            globals.push((g, src, out));
        }
    }
    // Drivers of the global networks come from the cells above, so mark
    // them once those are.
    let mut changed = true;
    while changed {
        changed = false;
        for &(_, src, out) in &globals {
            if d.is_driven(src) && !d.is_driven(out) {
                mark(&mut d, out);
                changed = true;
            }
        }
    }

    // Cells.
    for lc in &lcs {
        let bel = &arch.bels[lc.bel];
        let (x, y, z) = (bel.x as usize, bel.y as usize, bel.z as usize);
        let c = d.nl.add_cell(&format!("lc_x{x}_y{y}_z{z}"), LC);
        let mut init = 0u64;
        for (i, &p) in LUT_PERM.iter().enumerate() {
            if lc.bits[p] {
                init |= 1 << i;
            }
        }
        let neg = d.entry(x, y, "NegClk", None);
        let cin_set = z == 0 && d.entry(x, y, "CarryInSet", None);
        let params = [
            ("LUT_INIT", init),
            ("CARRY_ENABLE", lc.bits[8] as u64),
            ("DFF_ENABLE", lc.bits[9] as u64),
            ("SET_NORESET", lc.bits[18] as u64),
            ("ASYNC_SR", lc.bits[19] as u64),
            ("NEG_CLK", neg as u64),
            ("CIN_CONST", cin_set as u64),
            ("CIN_SET", cin_set as u64),
        ];
        for (k, v) in params {
            d.nl.set_param(c, k, Param::Int(v as i64));
        }
        for p in bel.pins.clone() {
            match p.name {
                "O" => d.pin(c, "O", Dir::Output, Some(p.node))?,
                "COUT" if lc.bits[8] => d.pin(c, "COUT", Dir::Output, Some(p.node))?,
                "I0" | "I1" | "I2" | "I3" | "CLK" | "CEN" | "SR" => {
                    d.pin(c, p.name, Dir::Input, Some(p.node))?
                }
                "CIN" if lc.bits[8] && !cin_set => d.pin(c, "CIN", Dir::Input, Some(p.node))?,
                _ => {}
            }
        }
    }
    for &bi in &rams {
        let bel = &arch.bels[bi];
        let (x, y) = (bel.x as usize, bel.y as usize);
        let c = d.nl.add_cell(&format!("ram_x{x}_y{y}"), RAM);
        let cbit =
            |d: &Decoder, i: usize| d.entry(x, y + 1, &format!("RamConfig.CBIT_{i}"), None) as u64;
        let wm = cbit(&d, 0) | cbit(&d, 1) << 1;
        let rm = cbit(&d, 2) | cbit(&d, 3) << 1;
        let (bottom, top) = (d.entry(x, y, "NegClk", None), d.entry(x, y + 1, "NegClk", None));
        let (nr, nw) = if is_1k { (top, bottom) } else { (bottom, top) };
        d.nl.set_param(c, "WRITE_MODE", Param::Int(wm as i64));
        d.nl.set_param(c, "READ_MODE", Param::Int(rm as i64));
        d.nl.set_param(c, "NEG_CLK_R", Param::Int(nr as i64));
        d.nl.set_param(c, "NEG_CLK_W", Param::Int(nw as i64));
        for word in 0..16 {
            let bits: Vec<Bit> = (0..256)
                .map(|i| if cfg.bram_bit(x, y, word * 256 + i) { Bit::One } else { Bit::Zero })
                .collect();
            d.nl.set_param(c, &format!("INIT_{word:X}"), Param::Bits(bits));
        }
        for p in bel.pins.clone() {
            let dir = if p.name.starts_with("RDATA") { Dir::Output } else { Dir::Input };
            d.pin(c, p.name, dir, Some(p.node))?;
        }
    }
    for &(g, src, out) in &globals {
        if !d.is_driven(src) {
            continue;
        }
        let c = d.nl.add_cell(&format!("gb{g}"), GB);
        d.pin(c, "USER_SIGNAL_TO_GLOBAL_BUFFER", Dir::Input, Some(src))?;
        d.pin(c, "GLOBAL_BUFFER_OUTPUT", Dir::Output, Some(out))?;
    }
    for (port, bi, pin_type, (x, y, z)) in ports {
        let bel = arch.bels[bi].clone();
        let node = |name: &str| bel.pins.iter().find(|p| p.name == name).map(|p| p.node);
        let (input, output) = (pin_type & 3, pin_type >> 2 & 15);
        if input == 1 && output == 0 {
            let c = d.nl.add_cell(&port, INPUT_PORT);
            d.pin(c, "PAD", Dir::Output, node("D_IN_0"))?;
        } else if output == 6 {
            let c = d.nl.add_cell(&port, OUTPUT_PORT);
            d.pin(c, "PAD", Dir::Input, node("D_OUT_0"))?;
        } else if pin_type != 0 {
            d.warnings.push(format!(
                "port {port} (pad {x},{y},{z}): PIN_TYPE {pin_type:06b} is not decoded"
            ));
        }
    }
    Ok((d.nl, d.warnings))
}
