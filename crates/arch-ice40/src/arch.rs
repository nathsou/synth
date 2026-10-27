//! The iCE40 architecture as seen by placement and routing: basic elements
//! (bels), their pins, and the routing graph with delays.

use std::fmt;

use synth_core::FxHashMap;

use crate::chipdb::{ChipDb, NodeId, Package, TileTemplate};
use crate::device::{Die, TileKind};

/// An iCE40 part: die, family variant and speed data.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    Lp384,
    Lp1k,
    Hx1k,
    Lp8k,
    Hx8k,
    Lp4k,
    Hx4k,
}

impl Part {
    pub const ALL: [Part; 7] =
        [Part::Lp384, Part::Lp1k, Part::Hx1k, Part::Lp8k, Part::Hx8k, Part::Lp4k, Part::Hx4k];

    pub fn name(self) -> &'static str {
        match self {
            Part::Lp384 => "lp384",
            Part::Lp1k => "lp1k",
            Part::Hx1k => "hx1k",
            Part::Lp8k => "lp8k",
            Part::Hx8k => "hx8k",
            Part::Lp4k => "lp4k",
            Part::Hx4k => "hx4k",
        }
    }

    pub fn die(self) -> Die {
        match self {
            Part::Lp384 => Die::D384,
            Part::Lp1k | Part::Hx1k => Die::D1k,
            _ => Die::D8k,
        }
    }

    /// 4K parts are 8K dies sold with fewer usable pins; their package
    /// tables carry a `:4k` suffix in IceStorm.
    pub fn package_suffix(self) -> &'static str {
        match self {
            Part::Lp4k | Part::Hx4k => ":4k",
            _ => "",
        }
    }
}

impl std::str::FromStr for Part {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let l = s.to_ascii_lowercase();
        Part::ALL.into_iter().find(|p| p.name() == l).ok_or_else(|| {
            format!(
                "unknown iCE40 part `{s}` (supported: {})",
                Part::ALL.map(|p| p.name()).join(", ")
            )
        })
    }
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Kind of a basic element.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BelKind {
    /// Logic cell: LUT4 + carry + flip-flop.
    Lc,
    Io,
    Ram,
    /// Global buffer driven from the fabric.
    Gb,
    /// Phase-locked loop (`SB_PLL40_CORE`).
    Pll,
}

/// A PLL site: its bel, the IO block its output A goes through (that IO is
/// then unusable), the global network the output drives, the tile its
/// `LOCK` output can reach (only that tile's logic cells), and its
/// configuration fields.
#[derive(Clone, Debug)]
pub struct PllSite {
    pub bel: BelId,
    pub out_io: (u8, u8, u8),
    pub global: u8,
    pub lock_tile: (u8, u8),
    /// The `PLL.PLLCONFIG_<n>` bit of each field bit (`DIVR_0`, ...), by
    /// tile.
    pub config: Vec<(String, u8, u8, String)>,
}

/// Netlist pin names of the PLL's fabric ports, by the IceStorm names of
/// their wires.
fn pll_pin(key: &str) -> Option<&'static str> {
    const PINS: [&str; 17] = [
        "REFERENCECLK",
        "RESETB",
        "BYPASS",
        "EXTFEEDBACK",
        "LATCHINPUTVALUE",
        "SCLK",
        "SDI",
        "SDO",
        "LOCK",
        "DYNAMICDELAY[0]",
        "DYNAMICDELAY[1]",
        "DYNAMICDELAY[2]",
        "DYNAMICDELAY[3]",
        "DYNAMICDELAY[4]",
        "DYNAMICDELAY[5]",
        "DYNAMICDELAY[6]",
        "DYNAMICDELAY[7]",
    ];
    let name = match key.strip_prefix("DYNAMICDELAY_") {
        Some(i) => format!("DYNAMICDELAY[{i}]"),
        None => key.to_string(),
    };
    PINS.iter().copied().find(|&p| p == name)
}

synth_core::define_id!(
    /// A basic element of the device.
    pub struct BelId
);

/// A bel pin and the routing node it attaches to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BelPin {
    pub name: &'static str,
    pub node: NodeId,
}

#[derive(Clone, Debug)]
pub struct Bel {
    pub kind: BelKind,
    pub x: u8,
    pub y: u8,
    pub z: u8,
    pub pins: Vec<BelPin>,
}

impl Bel {
    pub fn pin(&self, name: &str) -> Option<NodeId> {
        self.pins.iter().find(|p| p.name == name).map(|p| p.node)
    }
}

/// Coarse classification of routing nodes, used for delays, costs and the
/// router's lookahead.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum NodeClass {
    Global,
    Glb2Local,
    Local,
    LcInput,
    LcOutput,
    Carry,
    LcGlobal,
    Sp4H,
    Sp4V,
    Sp12H,
    Sp12V,
    IoSpan4,
    IoSpan12,
    Other,
}

impl NodeClass {
    fn of(name: &str) -> NodeClass {
        if name.starts_with("glb_netwk_") || name.starts_with("padin_") {
            NodeClass::Global
        } else if name.starts_with("glb2local") {
            NodeClass::Glb2Local
        } else if name.starts_with("local_") {
            NodeClass::Local
        } else if name.starts_with("sp4_h_") {
            NodeClass::Sp4H
        } else if name.starts_with("sp4_v_") || name.starts_with("sp4_r_v_") {
            NodeClass::Sp4V
        } else if name.starts_with("sp12_h_") {
            NodeClass::Sp12H
        } else if name.starts_with("sp12_v_") {
            NodeClass::Sp12V
        } else if name.starts_with("span4_") {
            NodeClass::IoSpan4
        } else if name.starts_with("span12_") {
            NodeClass::IoSpan12
        } else if name.ends_with("/cout") || name == "carry_in_mux" || name == "carry_in" {
            NodeClass::Carry
        } else if name.starts_with("lutff_global/")
            || name.starts_with("io_global/")
            || matches!(
                name,
                "ram/WCLK" | "ram/RCLK" | "ram/WCLKE" | "ram/RCLKE" | "ram/WE" | "ram/RE"
            )
        {
            NodeClass::LcGlobal
        } else if name.ends_with("/out") || name.contains("D_IN_") || name.starts_with("ram/RDATA")
        {
            NodeClass::LcOutput
        } else if name.contains("/in_")
            || name.contains("D_OUT_")
            || name.ends_with("OUT_ENB")
            || name.starts_with("ram/")
            || name == "fabout"
        {
            NodeClass::LcInput
        } else {
            NodeClass::Other
        }
    }
}

/// A programmable connection in the flattened routing graph.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PipRef {
    pub tile: u16,
    pub tpip: u16,
}

/// The device routing graph in compressed sparse row form.
#[derive(Debug)]
pub struct RoutingGraph {
    /// `edges[offsets[n]..offsets[n + 1]]` are the pips leaving node `n`.
    pub offsets: Vec<u32>,
    pub edge_dst: Vec<u32>,
    pub edge_pip: Vec<PipRef>,
    /// Intrinsic delay of each edge, in picoseconds: for edges driving span
    /// wires, the delay of the longest span (the actual one depends on how
    /// far along the wire the signal is used, see [`Arch::span_delay`]).
    pub edge_delay: Vec<f32>,
    /// Span wire kind each edge drives ([`SPAN4_H`] ...; 0 for none).
    pub edge_span: Vec<u8>,
    /// Representative position of each node (centre of its tiles).
    pub node_x: Vec<i16>,
    pub node_y: Vec<i16>,
    pub node_class: Vec<NodeClass>,
}

impl RoutingGraph {
    pub fn num_nodes(&self) -> usize {
        self.node_x.len()
    }

    #[inline]
    pub fn edges(&self, node: u32) -> std::ops::Range<usize> {
        self.offsets[node as usize] as usize..self.offsets[node as usize + 1] as usize
    }
}

/// Everything placement and routing need to know about one device.
#[derive(Debug)]
pub struct Arch {
    pub part: Part,
    pub db: &'static ChipDb,
    pub package: &'static Package,
    pub bels: Vec<Bel>,
    bel_at: FxHashMap<(u8, u8, u8, BelKind), BelId>,
    /// The PLLs usable in this package.
    pub plls: Vec<PllSite>,
    pub graph: RoutingGraph,
    /// Per template: delay of each template pip (ps), and the span kind it
    /// drives.
    pip_delays: Vec<Vec<(f32, u8)>>,
    /// Delay of span drivers by kind and distance (tiles) to the farthest
    /// use of the wire, as icetime models them.
    span_delays: [[f32; 13]; 5],
    /// LUT input permutation: per logic cell bel, its physical input pin
    /// nodes and the virtual nodes standing for the LUT's logical inputs
    /// (numbered after the device's nodes).
    lut_perm: Vec<LutPermutation>,
    /// Estimated delays between logic cells, searched when first needed.
    delays: std::sync::OnceLock<crate::estimate::DelayTable>,
}

/// A logic cell's physical LUT input pins, the virtual nodes of its logical
/// inputs, and its position.
type LutPermutation = ([NodeId; 4], [NodeId; 4], (u8, u8, u8));

/// Kinds of span wire drivers, whose delay depends on distance.
pub const SPAN4_H: u8 = 1;
pub const SPAN4_V: u8 = 2;
pub const SPAN12_H: u8 = 3;
pub const SPAN12_V: u8 = 4;

/// Graph edges marked with this tile are LUT input permutations, not pips.
pub const PERMUTATION_TILE: u16 = u16::MAX;

impl Arch {
    pub fn new(part: Part, package: &str) -> Result<Arch, String> {
        let db = ChipDb::get(part.die());
        let full = format!("{}{}", package.to_ascii_lowercase(), part.package_suffix());
        let pkg = db.package(&full).ok_or_else(|| {
            let names: Vec<&str> = db
                .packages
                .iter()
                .map(|p| p.name.as_str())
                .filter(|n| {
                    n.ends_with(part.package_suffix())
                        && (part.package_suffix() != "" || !n.contains(':'))
                })
                .collect();
            format!(
                "package `{package}` not available for {part} (available: {})",
                names.join(", ")
            )
        })?;
        let pip_delays = db.templates.iter().map(|t| template_pip_delays(db, t)).collect();
        let mut span_delays = [[0f32; 13]; 5];
        for (kind, cell, n) in [
            (SPAN4_H, "Span4Mux_h", 4),
            (SPAN4_V, "Span4Mux_v", 4),
            (SPAN12_H, "Span12Mux_h", 12),
            (SPAN12_V, "Span12Mux_v", 12),
        ] {
            for (d, v) in span_delays[kind as usize].iter_mut().enumerate().take(n + 1) {
                *v = db
                    .timing_arc(&format!("{cell}{d}"), crate::chipdb::ArcKind::IoPath, "I", "O")
                    .unwrap_or(0.0);
            }
        }
        let mut arch = Arch {
            part,
            db,
            package: pkg,
            bels: Vec::new(),
            bel_at: FxHashMap::default(),
            plls: Vec::new(),
            graph: RoutingGraph {
                offsets: vec![],
                edge_dst: vec![],
                edge_pip: vec![],
                edge_delay: vec![],
                edge_span: vec![],
                node_x: vec![],
                node_y: vec![],
                node_class: vec![],
            },
            pip_delays,
            span_delays,
            delays: std::sync::OnceLock::new(),
            lut_perm: Vec::new(),
        };
        arch.build_bels()?;
        arch.build_graph();
        Ok(arch)
    }

    pub fn bel(&self, id: BelId) -> &Bel {
        &self.bels[id.index()]
    }

    pub fn bel_at(&self, x: u8, y: u8, z: u8, kind: BelKind) -> Option<BelId> {
        self.bel_at.get(&(x, y, z, kind)).copied()
    }

    pub fn bels_of(&self, kind: BelKind) -> impl Iterator<Item = BelId> + '_ {
        self.bels
            .iter()
            .enumerate()
            .filter(move |(_, b)| b.kind == kind)
            .map(|(i, _)| BelId::new(i))
    }

    fn node(&self, x: u8, y: u8, name: &str) -> Result<NodeId, String> {
        self.db
            .node_by_name(x as usize, y as usize, name)
            .ok_or_else(|| format!("wire {name} missing in tile ({x},{y})"))
    }

    fn add_bel(&mut self, kind: BelKind, x: u8, y: u8, z: u8, pins: Vec<BelPin>) {
        let id = BelId::new(self.bels.len());
        self.bels.push(Bel { kind, x, y, z, pins });
        self.bel_at.insert((x, y, z, kind), id);
    }

    fn build_bels(&mut self) -> Result<(), String> {
        let db = self.db;
        for y in 0..db.grid_height() as u8 {
            for x in 0..db.grid_width() as u8 {
                match db.die.tile_kind(x as usize, y as usize) {
                    TileKind::Logic => {
                        for z in 0..8u8 {
                            const IN: [&str; 4] = ["I0", "I1", "I2", "I3"];
                            let mut pins = Vec::new();
                            for (k, name) in IN.iter().enumerate() {
                                pins.push(BelPin {
                                    name,
                                    node: self.node(x, y, &format!("lutff_{z}/in_{k}"))?,
                                });
                            }
                            pins.push(BelPin {
                                name: "O",
                                node: self.node(x, y, &format!("lutff_{z}/out"))?,
                            });
                            pins.push(BelPin {
                                name: "COUT",
                                node: self.node(x, y, &format!("lutff_{z}/cout"))?,
                            });
                            let cin = if z == 0 {
                                "carry_in_mux".to_string()
                            } else {
                                format!("lutff_{}/cout", z - 1)
                            };
                            pins.push(BelPin { name: "CIN", node: self.node(x, y, &cin)? });
                            pins.push(BelPin {
                                name: "CLK",
                                node: self.node(x, y, "lutff_global/clk")?,
                            });
                            pins.push(BelPin {
                                name: "CEN",
                                node: self.node(x, y, "lutff_global/cen")?,
                            });
                            pins.push(BelPin {
                                name: "SR",
                                node: self.node(x, y, "lutff_global/s_r")?,
                            });
                            // Logical LUT inputs, reachable from any physical
                            // input pin (the LUT's truth table follows).
                            let physical: [NodeId; 4] = std::array::from_fn(|k| pins[k].node);
                            let base = db.nodes.len() + 4 * self.lut_perm.len();
                            let virt: [NodeId; 4] = std::array::from_fn(|k| NodeId::new(base + k));
                            const PERM: [&str; 4] = ["PERM0", "PERM1", "PERM2", "PERM3"];
                            for (k, name) in PERM.iter().enumerate() {
                                pins.push(BelPin { name, node: virt[k] });
                            }
                            self.lut_perm.push((physical, virt, (x, y, z)));
                            self.add_bel(BelKind::Lc, x, y, z, pins);
                        }
                    }
                    TileKind::Io => {
                        for z in 0..2u8 {
                            let p = |s: &str| format!("io_{z}/{s}");
                            let pins = vec![
                                BelPin { name: "D_IN_0", node: self.node(x, y, &p("D_IN_0"))? },
                                BelPin { name: "D_IN_1", node: self.node(x, y, &p("D_IN_1"))? },
                                BelPin { name: "D_OUT_0", node: self.node(x, y, &p("D_OUT_0"))? },
                                BelPin { name: "D_OUT_1", node: self.node(x, y, &p("D_OUT_1"))? },
                                BelPin {
                                    name: "OUTPUT_ENABLE",
                                    node: self.node(x, y, &p("OUT_ENB"))?,
                                },
                                BelPin {
                                    name: "CLOCK_ENABLE",
                                    node: self.node(x, y, "io_global/cen")?,
                                },
                                BelPin {
                                    name: "INPUT_CLK",
                                    node: self.node(x, y, "io_global/inclk")?,
                                },
                                BelPin {
                                    name: "OUTPUT_CLK",
                                    node: self.node(x, y, "io_global/outclk")?,
                                },
                                BelPin {
                                    name: "LATCH_INPUT_VALUE",
                                    node: self.node(x, y, "io_global/latch")?,
                                },
                            ];
                            let mut pins = pins;
                            for (g, glb) in db.globals.iter().enumerate() {
                                if glb.pad == (x, y, z) {
                                    pins.push(BelPin {
                                        name: "GLOBAL_BUFFER_OUTPUT",
                                        node: self.node(x, y, &format!("glb_netwk_{g}"))?,
                                    });
                                }
                            }
                            self.add_bel(BelKind::Io, x, y, z, pins);
                        }
                    }
                    TileKind::RamB => {
                        let mut pins = Vec::new();
                        let find = |name: &str| {
                            db.node_by_name(x as usize, y as usize, name)
                                .or_else(|| db.node_by_name(x as usize, y as usize + 1, name))
                                .ok_or_else(|| format!("RAM wire {name} missing at ({x},{y})"))
                        };
                        for i in 0..16 {
                            pins.push(BelPin {
                                name: RAM_RDATA[i],
                                node: find(&format!("ram/RDATA_{i}"))?,
                            });
                            pins.push(BelPin {
                                name: RAM_WDATA[i],
                                node: find(&format!("ram/WDATA_{i}"))?,
                            });
                            pins.push(BelPin {
                                name: RAM_MASK[i],
                                node: find(&format!("ram/MASK_{i}"))?,
                            });
                        }
                        for i in 0..11 {
                            pins.push(BelPin {
                                name: RAM_RADDR[i],
                                node: find(&format!("ram/RADDR_{i}"))?,
                            });
                            pins.push(BelPin {
                                name: RAM_WADDR[i],
                                node: find(&format!("ram/WADDR_{i}"))?,
                            });
                        }
                        for name in ["RCLK", "RCLKE", "RE", "WCLK", "WCLKE", "WE"] {
                            pins.push(BelPin { name, node: find(&format!("ram/{name}"))? });
                        }
                        self.add_bel(BelKind::Ram, x, y, 0, pins);
                    }
                    TileKind::RamT | TileKind::Corner => {}
                }
            }
        }
        for g in 0..8 {
            let (x, y) = db.globals[g].fabout;
            let pins = vec![
                BelPin { name: "USER_SIGNAL_TO_GLOBAL_BUFFER", node: self.node(x, y, "fabout")? },
                BelPin {
                    name: "GLOBAL_BUFFER_OUTPUT",
                    node: self.node(x, y, &format!("glb_netwk_{g}"))?,
                },
            ];
            self.add_bel(BelKind::Gb, x, y, 2 + g as u8, pins);
        }
        for ec in db.extra_cells.iter().filter(|e| e.kind == "PLL") {
            if ec.locked.contains(&self.package.name) {
                continue;
            }
            let mut pins = Vec::new();
            let (mut out_io, mut lock_tile, mut config) = (None, None, Vec::new());
            for e in &ec.entries {
                if e.name.starts_with("PLLCONFIG_") {
                    config.push((e.key.clone(), e.x, e.y, e.name.clone()));
                } else if e.key == "PLLOUT_A" {
                    let z: u8 = e.name.parse().map_err(|_| "bad PLL output IO")?;
                    out_io = Some((e.x, e.y, z));
                    pins.push(BelPin {
                        name: "PLLOUTCORE",
                        node: self.node(e.x, e.y, &format!("io_{z}/D_IN_0"))?,
                    });
                } else if let Some(name) = pll_pin(&e.key) {
                    if name == "LOCK" {
                        lock_tile = Some((e.x, e.y));
                    }
                    pins.push(BelPin { name, node: self.node(e.x, e.y, &e.name)? });
                }
            }
            let out_io = out_io.ok_or("PLL without output A")?;
            let Some(global) = db.globals.iter().position(|g| g.pad == out_io) else {
                continue;
            };
            pins.push(BelPin {
                name: "PLLOUTGLOBAL",
                node: self.node(out_io.0, out_io.1, &format!("glb_netwk_{global}"))?,
            });
            let bel = BelId::new(self.bels.len());
            self.add_bel(BelKind::Pll, ec.x, ec.y, 0, pins);
            self.plls.push(PllSite {
                bel,
                out_io,
                global: global as u8,
                lock_tile: lock_tile.ok_or("PLL without LOCK")?,
                config,
            });
        }
        Ok(())
    }

    fn build_graph(&mut self) {
        let db = self.db;
        let n_dev = db.nodes.len();
        let n = n_dev + 4 * self.lut_perm.len();
        let mut count = vec![0u32; n + 1];
        let mut edges: Vec<(u32, u32, PipRef, (f32, u8))> = Vec::new();
        let gw = db.grid_width();
        let mut sum_x = vec![0i32; n];
        let mut sum_y = vec![0i32; n];
        let mut cnt = vec![0i32; n];
        for (physical, virt, (x, y, _)) in &self.lut_perm {
            for (k, v) in virt.iter().enumerate() {
                sum_x[v.index()] = *x as i32;
                sum_y[v.index()] = *y as i32;
                cnt[v.index()] = 1;
                for (j, p) in physical.iter().enumerate() {
                    count[p.index()] += 1;
                    let pip = PipRef { tile: PERMUTATION_TILE, tpip: (4 * j + k) as u16 };
                    edges.push((p.raw(), v.raw(), pip, (0.0, 0)));
                }
            }
        }
        for y in 0..db.grid_height() {
            for x in 0..gw {
                let t = db.tile_template[y * gw + x];
                if t == u8::MAX {
                    continue;
                }
                let tmpl = &db.templates[t as usize];
                for w in 0..tmpl.wires.len() as u16 {
                    if let Some(node) = db.tile_wire_node(x, y, w) {
                        sum_x[node.index()] += x as i32;
                        sum_y[node.index()] += y as i32;
                        cnt[node.index()] += 1;
                    }
                }
                for (i, p) in tmpl.pips.iter().enumerate() {
                    let sw = &tmpl.switches[p.switch as usize];
                    let (Some(src), Some(dst)) =
                        (db.tile_wire_node(x, y, p.src), db.tile_wire_node(x, y, sw.dst))
                    else {
                        continue;
                    };
                    count[src.index()] += 1;
                    let pip = PipRef { tile: (y * gw + x) as u16, tpip: i as u16 };
                    edges.push((src.raw(), dst.raw(), pip, self.pip_delays[t as usize][i]));
                }
            }
        }
        // Counting sort by source node keeps edge order deterministic.
        let mut offsets = vec![0u32; n + 1];
        for i in 0..n {
            offsets[i + 1] = offsets[i] + count[i];
        }
        let mut fill = offsets.clone();
        let m = edges.len();
        let mut edge_dst = vec![0u32; m];
        let mut edge_pip = vec![PipRef { tile: 0, tpip: 0 }; m];
        let mut edge_delay = vec![0f32; m];
        let mut edge_span = vec![0u8; m];
        for (src, dst, pip, (d, span)) in edges {
            let k = fill[src as usize] as usize;
            fill[src as usize] += 1;
            edge_dst[k] = dst;
            edge_pip[k] = pip;
            edge_delay[k] = d;
            edge_span[k] = span;
        }
        let node_class = (0..n)
            .map(|i| {
                if i < n_dev {
                    NodeClass::of(db.node_wire_name(NodeId::new(i)))
                } else {
                    NodeClass::LcInput
                }
            })
            .collect();
        self.graph = RoutingGraph {
            offsets,
            edge_dst,
            edge_pip,
            edge_delay,
            edge_span,
            node_x: (0..n).map(|i| (sum_x[i] / cnt[i].max(1)) as i16).collect(),
            node_y: (0..n).map(|i| (sum_y[i] / cnt[i].max(1)) as i16).collect(),
            node_class,
        };
    }

    /// The name of a routing graph node (`X<x>/Y<y>/<wire>`; logical LUT
    /// inputs are `lutff_<z>/in_<k>_lut`).
    pub fn node_name(&self, node: NodeId) -> String {
        let n_dev = self.db.nodes.len();
        if node.index() < n_dev {
            return self.db.node_name(node);
        }
        let i = node.index() - n_dev;
        let (_, _, (x, y, z)) = self.lut_perm[i / 4];
        format!("X{x}/Y{y}/lutff_{z}/in_{}_lut", i % 4)
    }

    /// For a LUT permutation edge: the physical input pin and the logical
    /// input it feeds.
    pub fn permutation(pip: PipRef) -> Option<(usize, usize)> {
        (pip.tile == PERMUTATION_TILE).then_some((pip.tpip as usize / 4, pip.tpip as usize % 4))
    }

    /// Estimated delays of unrouted connections between logic cells.
    pub fn delay_table(&self) -> &crate::estimate::DelayTable {
        self.delays.get_or_init(|| crate::estimate::DelayTable::new(self))
    }

    /// Delay of a span wire driver of `kind` whose wire is used up to
    /// `tiles` away from it.
    pub fn span_delay(&self, kind: u8, tiles: usize) -> f32 {
        let max = if kind <= SPAN4_V { 4 } else { 12 };
        self.span_delays[kind as usize][tiles.min(max)]
    }

    /// Tile coordinates of a routing graph edge's pip.
    pub fn pip_xy(&self, pip: PipRef) -> (usize, usize) {
        let gw = self.db.grid_width();
        (pip.tile as usize % gw, pip.tile as usize / gw)
    }

    /// The template pip behind a routing graph edge.
    pub fn pip_info(
        &self,
        pip: PipRef,
    ) -> (usize, usize, &TileTemplate, &crate::chipdb::TemplatePip) {
        let gw = self.db.grid_width();
        let (x, y) = (pip.tile as usize % gw, pip.tile as usize / gw);
        let t = self.db.template_at(x, y).unwrap();
        (x, y, t, &t.pips[pip.tpip as usize])
    }

    /// Package pin name → IO bel.
    pub fn pin_bel(&self, pin: &str) -> Option<BelId> {
        let p = self.package.pins.iter().find(|p| p.name.eq_ignore_ascii_case(pin))?;
        self.bel_at(p.x, p.y, p.z, BelKind::Io)
    }

    /// IO bel → package pin name, if bonded.
    pub fn bel_pin_name(&self, bel: BelId) -> Option<&str> {
        let b = self.bel(bel);
        self.package
            .pins
            .iter()
            .find(|p| (p.x, p.y, p.z) == (b.x, b.y, b.z))
            .map(|p| p.name.as_str())
    }
}

/// Names of RAM data pins, indexed by bit.
pub const RAM_RDATA: [&str; 16] = [
    "RDATA[0]",
    "RDATA[1]",
    "RDATA[2]",
    "RDATA[3]",
    "RDATA[4]",
    "RDATA[5]",
    "RDATA[6]",
    "RDATA[7]",
    "RDATA[8]",
    "RDATA[9]",
    "RDATA[10]",
    "RDATA[11]",
    "RDATA[12]",
    "RDATA[13]",
    "RDATA[14]",
    "RDATA[15]",
];
pub const RAM_WDATA: [&str; 16] = [
    "WDATA[0]",
    "WDATA[1]",
    "WDATA[2]",
    "WDATA[3]",
    "WDATA[4]",
    "WDATA[5]",
    "WDATA[6]",
    "WDATA[7]",
    "WDATA[8]",
    "WDATA[9]",
    "WDATA[10]",
    "WDATA[11]",
    "WDATA[12]",
    "WDATA[13]",
    "WDATA[14]",
    "WDATA[15]",
];
pub const RAM_MASK: [&str; 16] = [
    "MASK[0]", "MASK[1]", "MASK[2]", "MASK[3]", "MASK[4]", "MASK[5]", "MASK[6]", "MASK[7]",
    "MASK[8]", "MASK[9]", "MASK[10]", "MASK[11]", "MASK[12]", "MASK[13]", "MASK[14]", "MASK[15]",
];
pub const RAM_RADDR: [&str; 11] = [
    "RADDR[0]",
    "RADDR[1]",
    "RADDR[2]",
    "RADDR[3]",
    "RADDR[4]",
    "RADDR[5]",
    "RADDR[6]",
    "RADDR[7]",
    "RADDR[8]",
    "RADDR[9]",
    "RADDR[10]",
];
pub const RAM_WADDR: [&str; 11] = [
    "WADDR[0]",
    "WADDR[1]",
    "WADDR[2]",
    "WADDR[3]",
    "WADDR[4]",
    "WADDR[5]",
    "WADDR[6]",
    "WADDR[7]",
    "WADDR[8]",
    "WADDR[9]",
    "WADDR[10]",
];

/// Delay of each pip of a template, from the IceStorm timing models of the
/// routing multiplexers, keyed on the destination (and source) wire names.
fn template_pip_delays(db: &ChipDb, t: &TileTemplate) -> Vec<(f32, u8)> {
    let arc = |cell: &str, from: &str, to: &str| {
        db.timing_arc(cell, crate::chipdb::ArcKind::IoPath, from, to).unwrap_or(0.0)
    };
    let mux = |cell: &str| arc(cell, "I", "O");
    t.pips
        .iter()
        .map(|p| {
            let dst = t.wires[t.switches[p.switch as usize].dst as usize].as_str();
            let src = t.wires[p.src as usize].as_str();
            // Wire to wire: a span multiplexer, as slow as the distance the
            // signal then travels; from a cell output: an output driver.
            let wire = |w: &str| w.starts_with("sp") || w.starts_with("span");
            let span = if !wire(src) || !dst.starts_with("sp") {
                0
            } else if dst.starts_with("sp4_h_") && !src.starts_with("sp12_") {
                SPAN4_H
            } else if dst.starts_with("sp4_") && !src.starts_with("sp12_") {
                SPAN4_V
            } else if dst.starts_with("sp12_h_") {
                SPAN12_H
            } else if dst.starts_with("sp12_v_") {
                SPAN12_V
            } else {
                0
            };
            let delay = if !wire(src) && (dst.starts_with("sp4_") || dst.starts_with("span4_")) {
                mux("Odrv4")
            } else if !wire(src) && (dst.starts_with("sp12_") || dst.starts_with("span12_")) {
                mux("Odrv12")
            } else if dst.starts_with("sp4_") || dst.starts_with("span4_") {
                if src.starts_with("sp12_") || src.starts_with("span12_") {
                    mux("Sp12to4")
                } else if src.starts_with("span4_") {
                    mux("IoSpan4Mux")
                } else if dst.starts_with("sp4_h_") {
                    mux("Span4Mux_h4")
                } else {
                    mux("Span4Mux_v4")
                }
            } else if dst.starts_with("sp12_h_") || dst.starts_with("span12_horz") {
                mux("Span12Mux_h12")
            } else if dst.starts_with("sp12_v_") || dst.starts_with("span12_vert") {
                mux("Span12Mux_v12")
            } else if dst == "carry_in_mux" {
                arc("ICE_CARRY_IN_MUX", "carryinitin", "carryinitout")
            } else if matches!(
                dst,
                "lutff_global/clk"
                    | "io_global/inclk"
                    | "io_global/outclk"
                    | "ram/RCLK"
                    | "ram/WCLK"
            ) {
                mux("ClkMux")
            } else if matches!(dst, "lutff_global/s_r" | "io_global/latch" | "ram/RE" | "ram/WE") {
                mux("SRMux")
            } else if matches!(
                dst,
                "lutff_global/cen" | "io_global/cen" | "ram/RCLKE" | "ram/WCLKE"
            ) {
                mux("CEMux")
            } else if dst.starts_with("local_") {
                mux("LocalMux")
            } else if dst.starts_with("glb2local") {
                mux("Glb2LocalMux")
            } else if dst.starts_with("io_") && src.starts_with("local_") {
                mux("IoInMux")
            } else if dst.contains("/in_") || dst.starts_with("ram/") {
                mux("InMux")
            } else if dst == "fabout" {
                mux("IoInMux")
            } else {
                0.0
            };
            (delay, span)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hx8k_cb132() {
        let a = Arch::new(Part::Hx8k, "cb132").unwrap();
        assert_eq!(a.bels_of(BelKind::Lc).count(), 7680);
        assert_eq!(a.bels_of(BelKind::Ram).count(), 32);
        assert_eq!(a.bels_of(BelKind::Gb).count(), 8);
        let io = a.pin_bel("P7").unwrap();
        assert_eq!(a.bel(io).kind, BelKind::Io);
        assert_eq!(a.bel_pin_name(io), Some("P7"));
        assert!(a.graph.edge_dst.len() > 1_000_000);
        // Every bel pin node exists in the graph.
        for b in &a.bels {
            for p in &b.pins {
                assert!(p.node.index() < a.graph.num_nodes());
            }
        }
    }

    #[test]
    fn hx1k_vq100() {
        let a = Arch::new(Part::Hx1k, "vq100").unwrap();
        assert_eq!(a.bels_of(BelKind::Lc).count(), 1280);
        assert_eq!(a.bels_of(BelKind::Ram).count(), 16);
        assert!(Arch::new(Part::Hx1k, "nope").is_err());
    }
}
