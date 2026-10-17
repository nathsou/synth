//! The iCE40 device database: routing graph, configuration bit locations,
//! packages, global networks, hard cells and timing.
//!
//! The data comes from Project IceStorm's chip database (`icebox_chipdb`) and
//! timing files, converted offline by `synth-dbgen` into a compact binary blob
//! that is embedded in the library.
//!
//! # Routing model
//!
//! IceStorm names each electrical net once per tile it touches. We call such
//! a net a *node*. Tiles of the same *template* (logic, RAM bottom/top, and one
//! template per IO edge) have identical routing switches, so switches and
//! their programmable connections (*pips*) are stored once per template, in
//! terms of tile-local wire names. Each tile maps its local wires to global
//! nodes; a template pip exists in a tile when both of its wires do.

mod import;

pub use import::{ImportError, import};

use std::sync::OnceLock;

use synth_core::bin::{BinError, BinReader, BinWriter};
use synth_core::{FxHashMap, define_id};

use crate::device::{Die, TileKind};

define_id!(
    /// A routing node: one electrical net of the device.
    pub struct NodeId
);

/// Marks a tile-local wire that does not exist in a particular tile.
pub const NO_NODE: u32 = u32::MAX;

/// A configuration bit within a tile: `(row, column)` in IceStorm's `B<row>[<col>]` notation.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileBit {
    pub row: u8,
    pub col: u8,
}

/// A named group of configuration bits (`.logic_tile_bits` and similar).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigEntry {
    pub name: String,
    pub bits: Vec<TileBit>,
}

/// A routing multiplexer: drives `dst` from one of several sources selected
/// by a pattern over `bits`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switch {
    /// Tile-local wire index of the driven wire.
    pub dst: u16,
    /// IceStorm distinguishes buffered switches (`.buffer`) from routing
    /// switches (`.routing`, an all-zero pattern means off).
    pub routing: bool,
    pub bits: Vec<TileBit>,
}

/// A programmable connection of a template: `src` drives the switch's `dst`
/// when `bits[k]` equals bit `k` of `pattern` for every `k`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TemplatePip {
    pub switch: u16,
    pub src: u16,
    pub pattern: u16,
}

/// Routing and configuration data shared by all tiles of one template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileTemplate {
    /// `logic`, `ramb`, `ramt`, `io_l`, `io_r`, `io_b` or `io_t`.
    pub name: String,
    pub kind: TileKind,
    pub wires: Vec<String>,
    pub config: Vec<ConfigEntry>,
    pub switches: Vec<Switch>,
    pub pips: Vec<TemplatePip>,
}

impl TileTemplate {
    /// Finds a configuration entry by name.
    pub fn config(&self, name: &str) -> Option<&ConfigEntry> {
        self.config.iter().find(|c| c.name == name)
    }

    /// Finds a tile-local wire by name.
    pub fn wire(&self, name: &str) -> Option<u16> {
        self.wires.iter().position(|w| w == name).map(|i| i as u16)
    }
}

/// A node's canonical name: a tile-local wire in one of its tiles.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    pub x: u8,
    pub y: u8,
    pub wire: u16,
}

/// A package pin bonded to an IO block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagePin {
    pub name: String,
    pub x: u8,
    pub y: u8,
    pub z: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub pins: Vec<PackagePin>,
}

/// How one of the eight global networks can be driven.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct GlobalNetwork {
    /// IO tile whose `fabout` wire drives the network from the fabric.
    pub fabout: (u8, u8),
    /// IO block whose pad can drive the network directly.
    pub pad: (u8, u8, u8),
    /// CRAM bit `(bank, x, y)` enabling the pad-to-global connection.
    pub pad_extra_bit: (u8, u16, u16),
}

/// The IE/REN configuration bits of an IO block live in another (or the
/// same) IO block's slot: `io` is configured through `ieren`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IeRen {
    pub io: (u8, u8, u8),
    pub ieren: (u8, u8, u8),
}

/// One port or configuration field of a hard cell (PLL, warm boot).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraCellEntry {
    pub key: String,
    pub x: u8,
    pub y: u8,
    /// A tile-local wire name, a configuration entry name, or (for PLL
    /// outputs) the IO block index.
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraCell {
    /// `PLL` or `WARMBOOT`.
    pub kind: String,
    pub x: u8,
    pub y: u8,
    /// Packages on which this cell is not usable.
    pub locked: Vec<String>,
    pub entries: Vec<ExtraCellEntry>,
}

/// Kind of a timing arc in the IceStorm timing files.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArcKind {
    IoPath,
    Setup,
    Hold,
    Recovery,
    Removal,
}

/// A timing arc of a cell, in picoseconds (worst case of the corner).
#[derive(Clone, Debug, PartialEq)]
pub struct TimingArc {
    pub kind: ArcKind,
    pub from: String,
    pub to: String,
    pub rise: f32,
    pub fall: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimingCell {
    pub name: String,
    pub arcs: Vec<TimingArc>,
}

/// The complete database for one die.
#[derive(Clone, Debug, PartialEq)]
pub struct ChipDb {
    pub die: Die,
    pub templates: Vec<TileTemplate>,
    /// Template index of each grid tile (`y * grid_width + x`); `u8::MAX` for corners.
    pub tile_template: Vec<u8>,
    /// Start of each tile's slice in `tile_wire_nodes`.
    pub tile_wire_base: Vec<u32>,
    /// Node of each tile-local wire, or [`NO_NODE`].
    pub tile_wire_nodes: Vec<u32>,
    pub nodes: Vec<NodeInfo>,
    pub packages: Vec<Package>,
    pub globals: [GlobalNetwork; 8],
    /// IO tiles whose `fabout` drives the shared `io_global/latch` of an edge.
    pub iolatch: Vec<(u8, u8)>,
    pub ieren: Vec<IeRen>,
    pub extra_cells: Vec<ExtraCell>,
    /// Timing of the slow corner of the reference speed grade.
    pub timing: Vec<TimingCell>,
}

const MAGIC: &[u8; 8] = b"SYNTHi40";
const VERSION: u32 = 1;

impl ChipDb {
    /// The embedded database for `die`, decoded once and cached.
    pub fn get(die: Die) -> &'static ChipDb {
        static D384: OnceLock<ChipDb> = OnceLock::new();
        static D1K: OnceLock<ChipDb> = OnceLock::new();
        static D8K: OnceLock<ChipDb> = OnceLock::new();
        let (cell, blob): (&OnceLock<ChipDb>, &[u8]) = match die {
            Die::D384 => (&D384, include_bytes!("../../db/chipdb-384.bin")),
            Die::D1k => (&D1K, include_bytes!("../../db/chipdb-1k.bin")),
            Die::D8k => (&D8K, include_bytes!("../../db/chipdb-8k.bin")),
        };
        cell.get_or_init(|| {
            ChipDb::decode(blob).unwrap_or_else(|e| panic!("embedded {die} database: {e}"))
        })
    }

    pub fn grid_width(&self) -> usize {
        self.die.grid_width()
    }

    pub fn grid_height(&self) -> usize {
        self.die.grid_height()
    }

    /// The template of tile `(x, y)`, or `None` for corners.
    pub fn template_at(&self, x: usize, y: usize) -> Option<&TileTemplate> {
        let t = self.tile_template[y * self.grid_width() + x];
        (t != u8::MAX).then(|| &self.templates[t as usize])
    }

    /// The node of tile-local wire `wire` in tile `(x, y)`, if it exists there.
    #[inline]
    pub fn tile_wire_node(&self, x: usize, y: usize, wire: u16) -> Option<NodeId> {
        let tile = y * self.grid_width() + x;
        let base = self.tile_wire_base[tile] as usize;
        let n = self.tile_wire_nodes[base + wire as usize];
        (n != NO_NODE).then(|| NodeId::from_raw(n))
    }

    /// The node named `name` in tile `(x, y)`.
    pub fn node_by_name(&self, x: usize, y: usize, name: &str) -> Option<NodeId> {
        let t = self.template_at(x, y)?;
        self.tile_wire_node(x, y, t.wire(name)?)
    }

    /// The canonical name of a node, as `X<x>/Y<y>/<wire>`.
    pub fn node_name(&self, node: NodeId) -> String {
        let n = self.nodes[node.index()];
        let t = self.template_at(n.x as usize, n.y as usize).expect("node in corner tile");
        format!("X{}/Y{}/{}", n.x, n.y, t.wires[n.wire as usize])
    }

    /// The canonical tile-local wire name of a node (without position).
    pub fn node_wire_name(&self, node: NodeId) -> &str {
        let n = self.nodes[node.index()];
        let t = self.template_at(n.x as usize, n.y as usize).expect("node in corner tile");
        &t.wires[n.wire as usize]
    }

    pub fn package(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// Looks up a timing arc's worst-case delay in picoseconds.
    pub fn timing_arc(&self, cell: &str, kind: ArcKind, from: &str, to: &str) -> Option<f32> {
        let c = self.timing.iter().find(|c| c.name == cell)?;
        c.arcs
            .iter()
            .find(|a| a.kind == kind && a.from == from && a.to == to)
            .map(|a| a.rise.max(a.fall))
    }

    /// Builds a name → index map of a template's wires.
    pub fn wire_index(template: &TileTemplate) -> FxHashMap<&str, u16> {
        template.wires.iter().enumerate().map(|(i, w)| (w.as_str(), i as u16)).collect()
    }

    // ----- serialization -----

    pub fn encode(&self) -> Vec<u8> {
        let mut w = BinWriter::new();
        w.raw(MAGIC);
        w.u32_fixed(VERSION);
        w.str(self.die.name());
        let bits = |w: &mut BinWriter, bits: &Vec<TileBit>| {
            w.seq(bits, |w, b| {
                w.u8(b.row);
                w.u8(b.col);
            })
        };
        w.seq(&self.templates, |w, t| {
            w.str(&t.name);
            w.u8(TileKind::ALL.iter().position(|k| *k == t.kind).unwrap() as u8);
            w.seq(&t.wires, |w, s| w.str(s));
            w.seq(&t.config, |w, c| {
                w.str(&c.name);
                bits(w, &c.bits);
            });
            w.seq(&t.switches, |w, s| {
                w.uvar(s.dst as u64);
                w.bool(s.routing);
                bits(w, &s.bits);
            });
            w.seq(&t.pips, |w, p| {
                w.uvar(p.switch as u64);
                w.uvar(p.src as u64);
                w.uvar(p.pattern as u64);
            });
        });
        w.seq(&self.tile_template, |w, &t| w.u8(t));
        w.seq(&self.tile_wire_base, |w, &b| w.uvar(b as u64));
        // Node ids of neighbouring wires are close; store deltas.
        let mut prev = 0i64;
        w.seq(&self.tile_wire_nodes, |w, &n| {
            let v = if n == NO_NODE { -1 } else { n as i64 };
            w.ivar(v - prev);
            prev = v;
        });
        w.seq(&self.nodes, |w, n| {
            w.u8(n.x);
            w.u8(n.y);
            w.uvar(n.wire as u64);
        });
        w.seq(&self.packages, |w, p| {
            w.str(&p.name);
            w.seq(&p.pins, |w, pin| {
                w.str(&pin.name);
                w.u8(pin.x);
                w.u8(pin.y);
                w.u8(pin.z);
            });
        });
        for g in &self.globals {
            w.u8(g.fabout.0);
            w.u8(g.fabout.1);
            w.u8(g.pad.0);
            w.u8(g.pad.1);
            w.u8(g.pad.2);
            w.u8(g.pad_extra_bit.0);
            w.uvar(g.pad_extra_bit.1 as u64);
            w.uvar(g.pad_extra_bit.2 as u64);
        }
        w.seq(&self.iolatch, |w, &(x, y)| {
            w.u8(x);
            w.u8(y);
        });
        w.seq(&self.ieren, |w, e| {
            for v in [e.io.0, e.io.1, e.io.2, e.ieren.0, e.ieren.1, e.ieren.2] {
                w.u8(v);
            }
        });
        w.seq(&self.extra_cells, |w, c| {
            w.str(&c.kind);
            w.u8(c.x);
            w.u8(c.y);
            w.seq(&c.locked, |w, s| w.str(s));
            w.seq(&c.entries, |w, e| {
                w.str(&e.key);
                w.u8(e.x);
                w.u8(e.y);
                w.str(&e.name);
            });
        });
        w.seq(&self.timing, |w, c| {
            w.str(&c.name);
            w.seq(&c.arcs, |w, a| {
                w.u8(a.kind as u8);
                w.str(&a.from);
                w.str(&a.to);
                w.f32(a.rise);
                w.f32(a.fall);
            });
        });
        w.into_bytes()
    }

    pub fn decode(data: &[u8]) -> Result<ChipDb, BinError> {
        let mut r = BinReader::new(data);
        let bad = |offset, message| BinError { offset, message };
        if r.raw(8)? != MAGIC {
            return Err(bad(0, "not an iCE40 chip database"));
        }
        if r.u32_fixed()? != VERSION {
            return Err(bad(8, "unsupported chip database version"));
        }
        let die: Die = r.str()?.parse().map_err(|_| bad(12, "unknown die"))?;
        let bits = |r: &mut BinReader| r.seq(|r| Ok(TileBit { row: r.u8()?, col: r.u8()? }));
        let templates = r.seq(|r| {
            let name = r.string()?;
            let kind = *TileKind::ALL.get(r.u8()? as usize).ok_or(bad(0, "bad tile kind"))?;
            let wires = r.seq(|r| r.string())?;
            let config = r.seq(|r| Ok(ConfigEntry { name: r.string()?, bits: bits(r)? }))?;
            let switches =
                r.seq(|r| Ok(Switch { dst: r.u16()?, routing: r.bool()?, bits: bits(r)? }))?;
            let pips =
                r.seq(|r| Ok(TemplatePip { switch: r.u16()?, src: r.u16()?, pattern: r.u16()? }))?;
            Ok(TileTemplate { name, kind, wires, config, switches, pips })
        })?;
        let tile_template = r.seq(|r| r.u8())?;
        let tile_wire_base = r.seq(|r| r.u32())?;
        let mut prev = 0i64;
        let tile_wire_nodes = r.seq(|r| {
            let v = prev + r.ivar()?;
            prev = v;
            Ok(if v < 0 { NO_NODE } else { v as u32 })
        })?;
        let nodes = r.seq(|r| Ok(NodeInfo { x: r.u8()?, y: r.u8()?, wire: r.u16()? }))?;
        let packages = r.seq(|r| {
            Ok(Package {
                name: r.string()?,
                pins: r.seq(|r| {
                    Ok(PackagePin { name: r.string()?, x: r.u8()?, y: r.u8()?, z: r.u8()? })
                })?,
            })
        })?;
        let mut globals = [GlobalNetwork::default(); 8];
        for g in &mut globals {
            g.fabout = (r.u8()?, r.u8()?);
            g.pad = (r.u8()?, r.u8()?, r.u8()?);
            g.pad_extra_bit = (r.u8()?, r.u16()?, r.u16()?);
        }
        let iolatch = r.seq(|r| Ok((r.u8()?, r.u8()?)))?;
        let ieren = r.seq(|r| {
            Ok(IeRen { io: (r.u8()?, r.u8()?, r.u8()?), ieren: (r.u8()?, r.u8()?, r.u8()?) })
        })?;
        let extra_cells = r.seq(|r| {
            Ok(ExtraCell {
                kind: r.string()?,
                x: r.u8()?,
                y: r.u8()?,
                locked: r.seq(|r| r.string())?,
                entries: r.seq(|r| {
                    Ok(ExtraCellEntry {
                        key: r.string()?,
                        x: r.u8()?,
                        y: r.u8()?,
                        name: r.string()?,
                    })
                })?,
            })
        })?;
        let timing = r.seq(|r| {
            Ok(TimingCell {
                name: r.string()?,
                arcs: r.seq(|r| {
                    let kind = match r.u8()? {
                        0 => ArcKind::IoPath,
                        1 => ArcKind::Setup,
                        2 => ArcKind::Hold,
                        3 => ArcKind::Recovery,
                        4 => ArcKind::Removal,
                        _ => return Err(bad(0, "bad arc kind")),
                    };
                    Ok(TimingArc {
                        kind,
                        from: r.string()?,
                        to: r.string()?,
                        rise: r.f32()?,
                        fall: r.f32()?,
                    })
                })?,
            })
        })?;
        if !r.at_end() {
            return Err(bad(data.len(), "trailing data"));
        }
        let db = ChipDb {
            die,
            templates,
            tile_template,
            tile_wire_base,
            tile_wire_nodes,
            nodes,
            packages,
            globals,
            iolatch,
            ieren,
            extra_cells,
            timing,
        };
        db.validate().map_err(|m| bad(0, m))?;
        Ok(db)
    }

    /// Structural consistency checks so that later indexing cannot panic.
    fn validate(&self) -> Result<(), &'static str> {
        let tiles = self.grid_width() * self.grid_height();
        if self.tile_template.len() != tiles || self.tile_wire_base.len() != tiles {
            return Err("tile table size mismatch");
        }
        for (i, &t) in self.tile_template.iter().enumerate() {
            if t == u8::MAX {
                continue;
            }
            let tmpl = self.templates.get(t as usize).ok_or("bad template index")?;
            let base = self.tile_wire_base[i] as usize;
            let wires =
                self.tile_wire_nodes.get(base..base + tmpl.wires.len()).ok_or("bad base")?;
            if wires.iter().any(|&n| n != NO_NODE && n as usize >= self.nodes.len()) {
                return Err("bad node index");
            }
        }
        for t in &self.templates {
            for s in &t.switches {
                if s.dst as usize >= t.wires.len() || s.bits.len() > 16 {
                    return Err("bad switch");
                }
            }
            for p in &t.pips {
                if p.switch as usize >= t.switches.len() || p.src as usize >= t.wires.len() {
                    return Err("bad pip");
                }
            }
        }
        for n in &self.nodes {
            let t = self.template_at(n.x as usize, n.y as usize).ok_or("node in corner")?;
            if n.wire as usize >= t.wires.len() {
                return Err("bad node wire");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_databases_load() {
        for die in Die::ALL {
            let db = ChipDb::get(die);
            assert_eq!(db.die, die);
            assert_eq!(db.encode(), ChipDb::encode(&ChipDb::decode(&db.encode()).unwrap()));
        }
        let db = ChipDb::get(Die::D1k);
        let n = db.node_by_name(1, 1, "lutff_0/out").unwrap();
        assert_eq!(db.node_name(n), "X1/Y1/lutff_0/out");
        assert!(db.package("vq100").is_some());
        assert!(db.timing_arc("LogicCell40", ArcKind::IoPath, "in0", "lcout").unwrap() > 100.0);
    }
}
