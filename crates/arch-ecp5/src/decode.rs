//! A chip configuration back to a netlist of the primitives it configures
//! (the reverse of [`crate::bitgen`]), to simulate what a bitstream
//! implements, as Project Trellis' `ecp_vlog` reads one.
//!
//! Wires join through every configured switch; through the switch a
//! multiplexer takes when none of its bits is set (its zero-bit arc), where
//! none of its other switches is configured; through every fixed
//! connection and wire link of the clock network; and through the clock
//! buffers, which pass their input on unless gated. The slices of every
//! configured logic tile become `LUT4`s or `CCU2C`s and `TRELLIS_FF`s on
//! the joined wires, with their settings read back from the tile (the
//! database's defaults where the configuration leaves them). The ports
//! named in `pins` become the netlist's ports at their pads. Inputs on
//! wires nothing drives are left unconnected (a slice reads them high),
//! and routing inputs tied to a constant become LUTs driving it. Block
//! RAMs in use become `DP16KD`s with their initial contents. Distributed
//! RAM, wide multiplexers and I/O registers are not decoded.

use std::collections::BTreeMap;

use synth_core::FxHashMap;
use synth_netlist::{Bit, CellId, Dir, INPUT_PORT, NetId, Netlist, OUTPUT_PORT, Param};

use crate::arch::Arch;
use crate::bits::Cram;
use crate::cells::{CCU2C, DP16KD, FF, LUT4};
use crate::config::{ChipConfig, TileConfig};
use crate::db::TileInfo;
use crate::graph::NO_TILE;

const SLICES: [char; 4] = ['A', 'B', 'C', 'D'];
const LUT_PINS: [&str; 4] = ["A", "B", "C", "D"];

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

/// The carry wires into and out of slice `s` of a logic tile.
fn carry_wires(s: usize) -> (&'static str, &'static str) {
    [
        ("FCI_SLICE", "FCOA_SLICE"),
        ("FCIB_SLICE", "FCOB_SLICE"),
        ("FCIC_SLICE", "FCOC_SLICE"),
        ("FCID_SLICE", "FCO_SLICE"),
    ][s]
}

/// Whether a fixed edge is one of the graph's LUT input permutations
/// rather than a wire of the device.
fn is_permutation(src: &str, dst: &str) -> bool {
    let (s, d) = (src.as_bytes(), dst.as_bytes());
    s.len() == 2
        && dst.len() == 8
        && dst.ends_with("_SLICE")
        && b"ABCD".contains(&s[0])
        && b"ABCD".contains(&d[0])
        && s[0] != d[0]
        && s[1] == d[1]
}

/// A cell to create: its kind, name, parameters and pins (with the wires
/// they are on, at the cell's tile).
struct Cell {
    kind: &'static str,
    name: String,
    params: Vec<(String, Param)>,
    inputs: Vec<(String, u32)>,
    outputs: Vec<(String, u32)>,
}

struct Decoder<'a> {
    arch: &'a Arch,
    cram: &'a Cram,
    /// The configuration read back tile by tile.
    cc: ChipConfig,
    bram_init: &'a BTreeMap<u16, Vec<u16>>,
    uf: Uf,
    warnings: Vec<String>,
}

impl Decoder<'_> {
    fn config(&self, tile: &TileInfo) -> Option<&TileConfig> {
        self.cc.tiles.get(&tile.name)
    }

    /// The value of enum `name` of `tile`: as configured, else the
    /// database's default.
    fn enum_value(&self, tile: &TileInfo, name: &str) -> Result<String, String> {
        if let Some(v) = self.config(tile).and_then(|c| c.enums.iter().find(|e| e.0 == name)) {
            return Ok(v.1.clone());
        }
        let bits = self.arch.db.tile_bits(&tile.ttype)?;
        Ok(bits.enums.get(name).and_then(|e| e.default.clone()).unwrap_or_default())
    }

    /// The value of word `name` of `tile`, least significant bit first.
    fn word(&self, tile: &TileInfo, name: &str) -> Result<u64, String> {
        let value = match self.config(tile).and_then(|c| c.words.iter().find(|w| w.0 == name)) {
            Some(w) => w.1.clone(),
            None => {
                let bits = self.arch.db.tile_bits(&tile.ttype)?;
                bits.words.get(name).map(|w| w.default.clone()).unwrap_or_default()
            }
        };
        Ok(value.iter().enumerate().map(|(i, &b)| (b as u64) << i).sum())
    }

    fn wire(&self, tile: &TileInfo, name: &str) -> Result<u32, String> {
        self.arch.node(tile.col, tile.row, name)
    }

    /// Which of the tile's two shared wires `base0`/`base1` (clock or
    /// set/reset) reaches slice wire `slice_wire`.
    fn shared(
        &mut self,
        tile: &TileInfo,
        base: &str,
        slice_wire: u32,
    ) -> Result<Option<usize>, String> {
        let r = self.uf.find(slice_wire);
        for i in 0..2 {
            let w = self.wire(tile, &format!("{base}{i}"))?;
            if self.uf.find(w) == r {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    /// The cells of a configured logic tile.
    fn slices(&mut self, tile: &TileInfo, cells: &mut Vec<Cell>) -> Result<(), String> {
        let (x, y) = (tile.col, tile.row);
        for (s, l) in SLICES.iter().enumerate() {
            let slice = format!("SLICE{l}");
            let mode = self.enum_value(tile, &format!("{slice}.MODE"))?;
            // A LUT input tied high reads 1 whatever its wire carries.
            let lut_inputs =
                |d: &Self, k: usize, suffix: &str| -> Result<Vec<(String, u32)>, String> {
                    let z = 2 * s + k;
                    let mut ins = Vec::new();
                    for p in LUT_PINS {
                        if d.enum_value(tile, &format!("{slice}.{p}{k}MUX"))? == "1" {
                            continue;
                        }
                        ins.push((format!("{p}{suffix}"), d.wire(tile, &format!("{p}{z}_SLICE"))?));
                    }
                    Ok(ins)
                };
            match mode.as_str() {
                "LOGIC" => {
                    for k in 0..2 {
                        let z = 2 * s + k;
                        let init = self.word(tile, &format!("{slice}.K{k}.INIT"))?;
                        cells.push(Cell {
                            kind: LUT4,
                            name: format!("lut_x{x}_y{y}_{z}"),
                            params: vec![("INIT".into(), Param::Int(init as i64))],
                            inputs: lut_inputs(self, k, "")?,
                            outputs: vec![("Z".into(), self.wire(tile, &format!("F{z}_SLICE"))?)],
                        });
                    }
                }
                "CCU2" => {
                    let mut params = Vec::new();
                    let mut inputs = Vec::new();
                    for h in 0..2 {
                        let init = self.word(tile, &format!("{slice}.K{h}.INIT"))?;
                        params.push((format!("INIT{h}"), Param::Int(init as i64)));
                        let inject = self.enum_value(tile, &format!("{slice}.CCU2.INJECT1_{h}"))?;
                        params.push((format!("INJECT1_{h}"), Param::Str(inject)));
                        inputs.extend(lut_inputs(self, h, &h.to_string())?);
                    }
                    let (cin, cout) = carry_wires(s);
                    inputs.push(("CIN".into(), self.wire(tile, cin)?));
                    cells.push(Cell {
                        kind: CCU2C,
                        name: format!("ccu2_x{x}_y{y}_{l}"),
                        params,
                        inputs,
                        outputs: vec![
                            ("S0".into(), self.wire(tile, &format!("F{}_SLICE", 2 * s))?),
                            ("S1".into(), self.wire(tile, &format!("F{}_SLICE", 2 * s + 1))?),
                            ("COUT".into(), self.wire(tile, cout)?),
                        ],
                    });
                }
                other => {
                    self.warnings
                        .push(format!("{}: {slice} in mode {other} is not decoded", tile.name));
                    continue;
                }
            }
            // Flip-flops: clock, set/reset and enable are shared by the slice.
            let clk = self.wire(tile, &format!("CLK{s}_SLICE"))?;
            let lsr = self.wire(tile, &format!("LSR{s}_SLICE"))?;
            let clkmux = match self.shared(tile, "CLK", clk)? {
                Some(i) => self.enum_value(tile, &format!("CLK{i}.CLKMUX"))?,
                None => "CLK".into(),
            };
            let (lsrmux, srmode) = match self.shared(tile, "LSR", lsr)? {
                Some(i) => (
                    self.enum_value(tile, &format!("LSR{i}.LSRMUX"))?,
                    self.enum_value(tile, &format!("LSR{i}.SRMODE"))?,
                ),
                None => ("LSR".into(), "LSR_OVER_CE".into()),
            };
            let common = [
                ("GSR", self.enum_value(tile, &format!("{slice}.GSR"))?),
                ("CEMUX", self.enum_value(tile, &format!("{slice}.CEMUX"))?),
                ("CLKMUX", clkmux),
                ("LSRMUX", lsrmux),
                ("SRMODE", srmode),
            ];
            for k in 0..2 {
                let z = 2 * s + k;
                let reg = format!("{slice}.REG{k}");
                if self.enum_value(tile, &format!("{reg}.LSRMODE"))? == "PRLD" {
                    self.warnings.push(format!("{}: {reg} preload is not decoded", tile.name));
                }
                let mut params: Vec<(String, Param)> =
                    common.iter().map(|(k, v)| (k.to_string(), Param::Str(v.clone()))).collect();
                params.push((
                    "REGSET".into(),
                    Param::Str(self.enum_value(tile, &format!("{reg}.REGSET"))?),
                ));
                // `SD` 1 takes the slice's own LUT output, 0 the bypass `M`.
                let di = if self.enum_value(tile, &format!("{reg}.SD"))? == "1" {
                    format!("DI{z}_SLICE")
                } else {
                    format!("M{z}_SLICE")
                };
                cells.push(Cell {
                    kind: FF,
                    name: format!("ff_x{x}_y{y}_{z}"),
                    params,
                    inputs: vec![
                        ("DI".into(), self.wire(tile, &di)?),
                        ("CLK".into(), clk),
                        ("LSR".into(), lsr),
                        ("CE".into(), self.wire(tile, &format!("CE{s}_SLICE"))?),
                    ],
                    outputs: vec![("Q".into(), self.wire(tile, &format!("Q{z}_SLICE"))?)],
                });
            }
        }
        Ok(())
    }

    /// Enum `name` of a group of tiles, which each hold some of its bits:
    /// the value whose bits differ least from the configuration's, then
    /// with the most bits. (Values do not always match exactly: the
    /// database lists some bits of other settings among a value's, such as
    /// a write enable's inversion among those of the block RAM's mode.)
    fn group_enum(&self, tiles: &[&TileInfo], name: &str) -> Result<Option<String>, String> {
        let mut score: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        let mut default = None;
        for t in tiles {
            let bits = self.arch.db.tile_bits(&t.ttype)?;
            let Some(e) = bits.enums.get(name) else { continue };
            default = Some(e.default.clone().unwrap_or_default());
            for (value, group) in &e.options {
                let s = score.entry(value.clone()).or_default();
                s.0 += group.mismatches(self.cram, &t.window);
                s.1 += group.0.len();
            }
        }
        let Some(default) = default else { return Ok(None) };
        let best = score.into_iter().min_by_key(|s| (s.1.0, std::cmp::Reverse(s.1.1)));
        Ok(Some(best.map_or(default, |s| s.0)))
    }

    /// Word `name` of a group of tiles, least significant bit first: each
    /// bit from the tile that holds it.
    fn group_word(&self, tiles: &[&TileInfo], name: &str) -> Result<Option<u64>, String> {
        let mut value = None;
        for t in tiles {
            let bits = self.arch.db.tile_bits(&t.ttype)?;
            let Some(w) = bits.words.get(name) else { continue };
            let v = value.get_or_insert(0u64);
            for (i, g) in w.bits.iter().enumerate() {
                if !g.0.is_empty() && g.matches(self.cram, &t.window) {
                    *v |= 1 << i;
                }
            }
        }
        Ok(value)
    }

    /// The block RAM of the tile at `(x, y)` holding bel `EBR<z>`, if in use.
    fn ebr(&mut self, x: usize, y: usize, z: usize, cells: &mut Vec<Cell>) -> Result<(), String> {
        let names = crate::bitgen::ebr_tiles(self.arch, x, y, z)?;
        let tiles: Vec<&TileInfo> = names.iter().filter_map(|n| self.arch.dev.tile(n)).collect();
        let ebr = format!("EBR{z}");
        let mode = self.group_enum(&tiles, &format!("{ebr}.MODE"))?.unwrap_or_default();
        if mode == "NONE" || mode == "_NONE_" {
            return Ok(());
        }
        let name = format!("ebr_x{x}_y{y}_{z}");
        if mode != "DP16KD" {
            self.warnings.push(format!("{name}: block RAM mode {mode} is not decoded"));
            return Ok(());
        }
        let mut params: Vec<(String, Param)> = Vec::new();
        for p in ["A", "B"] {
            for (key, setting) in [
                (format!("DATA_WIDTH_{p}"), format!("DP16KD.DATA_WIDTH_{p}")),
                (format!("WRITEMODE_{p}"), format!("DP16KD.WRITEMODE_{p}")),
                (format!("REGMODE_{p}"), format!("REGMODE_{p}")),
            ] {
                if let Some(v) = self.group_enum(&tiles, &format!("{ebr}.{setting}"))? {
                    params.push((key, Param::Str(v)));
                }
            }
            // Stored most significant bit first.
            let csd = self.group_word(&tiles, &format!("{ebr}.CSDECODE_{p}"))?.unwrap_or(0);
            let csd = format!("0b{}{}{}", csd & 1, csd >> 1 & 1, csd >> 2 & 1);
            params.push((format!("CSDECODE_{p}"), Param::Str(csd)));
        }
        if let Some(v) = self.group_enum(&tiles, &format!("{ebr}.RESETMODE"))? {
            params.push(("RESETMODE".into(), Param::Str(v)));
        }
        let mut inputs = Vec::new();
        let mut pins: Vec<String> = Vec::new();
        for p in ["A", "B"] {
            pins.extend((0..14).map(|i| format!("AD{p}{i}")));
            pins.extend((0..18).map(|i| format!("DI{p}{i}")));
            pins.extend((0..3).map(|i| format!("CS{p}{i}")));
            pins.extend(["CLK", "CE", "OCE", "WE", "RST"].map(|s| format!("{s}{p}")));
        }
        for pin in pins {
            // The block's own inversions of its inputs.
            if let Some(v) = self.group_enum(&tiles, &format!("{ebr}.{pin}MUX"))? {
                params.push((
                    format!("{pin}MUX"),
                    Param::Str(if v == "INV" { v } else { pin.clone() }),
                ));
            }
            let w = self.arch.node(x, y, &format!("J{pin}_EBR"))?;
            inputs.push((pin, w));
        }
        let wid = self.group_word(&tiles, &format!("{ebr}.WID"))?.unwrap_or(0);
        let wid = (0..9).filter(|i| wid >> i & 1 == 1).map(|i| 1u16 << (8 - i)).sum::<u16>();
        let units = self.bram_init.get(&wid).cloned().unwrap_or_else(|| vec![0; 2048]);
        for k in 0..64 {
            let mut bits = vec![Bit::Zero; 320];
            for i in 0..16 {
                let w = 16 * k + i;
                let v = units[2 * w] as u32 | (units[2 * w + 1] as u32) << 9;
                for t in 0..18 {
                    if v >> t & 1 == 1 {
                        bits[20 * i + t] = Bit::One;
                    }
                }
            }
            params.push((format!("INITVAL_{k:02X}"), Param::Bits(bits)));
        }
        let mut outputs = Vec::new();
        for p in ["A", "B"] {
            for i in 0..18 {
                outputs
                    .push((format!("DO{p}{i}"), self.arch.node(x, y, &format!("JDO{p}{i}_EBR"))?));
            }
        }
        cells.push(Cell { kind: DP16KD, name, params, inputs, outputs });
        Ok(())
    }

    /// The constants a routing tile's input multiplexers tie wires to
    /// (`CIB.<wire>MUX` `0` or `1`). Clock and set/reset inputs only tie
    /// with `0`, which nextpnr takes as high (inverting at the block where
    /// it needs low), and so it is read here.
    fn ties(&self, tile: &TileInfo, cells: &mut Vec<Cell>) -> Result<(), String> {
        let Some(tc) = self.config(tile) else { return Ok(()) };
        for (name, v) in &tc.enums {
            let Some(wire) = name.strip_prefix("CIB.").and_then(|n| n.strip_suffix("MUX")) else {
                continue;
            };
            let high = match v.as_str() {
                "1" => true,
                "0" => wire.starts_with("JCLK") || wire.starts_with("JLSR"),
                _ => continue,
            };
            let w = self.wire(tile, wire)?;
            cells.push(Cell {
                kind: LUT4,
                name: format!("tie_{}_{wire}", tile.name.replace(':', "_")),
                params: vec![("INIT".into(), Param::Int(if high { 0xffff } else { 0 }))],
                inputs: Vec::new(),
                outputs: vec![("Z".into(), w)],
            });
        }
        Ok(())
    }
}

/// Decodes the configuration memory `cram` (with block RAM contents
/// `bram_init`) for `arch`, with port bits `pins` (name, pad `(x, y, PIO
/// letter)`). Returns the netlist and warnings about what was not decoded.
pub fn decode(
    arch: &Arch,
    cram: &Cram,
    bram_init: &BTreeMap<u16, Vec<u16>>,
    pins: &[(String, (usize, usize, char))],
) -> Result<(Netlist, Vec<String>), String> {
    let g = &arch.graph;
    let n = g.num_nodes();
    let prefix = crate::graph::size_prefix(&arch.dev.info.name);
    let cc = arch.dev.from_cram(&arch.db, cram)?;
    let mut d = Decoder {
        arch,
        cram,
        cc,
        bram_init,
        uf: Uf((0..n as u32).collect()),
        warnings: Vec::new(),
    };
    let cc = &d.cc.clone();
    // Configured switches. In carry mode, a slice's output multiplexers
    // share their bits with the carry settings, and stay on the outputs.
    for (name, tc) in &cc.tiles {
        let Some(tile) = arch.dev.tile(name) else {
            d.warnings.push(format!("no tile {name} on {}", arch.dev.info.name));
            continue;
        };
        for (sink, source) in &tc.arcs {
            let mut source = source.clone();
            if tile.ttype == "PLC2"
                && sink.len() == 2
                && sink.starts_with('F')
                && (source.starts_with("F5") || source.starts_with("FX"))
                && source.ends_with("_SLICE")
            {
                let slice = format!("SLICE{}.MODE", &source[2..3]);
                if d.enum_value(tile, &slice)? == "CCU2" {
                    source = format!("{sink}_SLICE");
                }
            }
            let (a, b) = (
                g.tile_wire(tile.col, tile.row, &source, prefix),
                g.tile_wire(tile.col, tile.row, sink, prefix),
            );
            match (a, b) {
                (Some(a), Some(b)) => d.uf.union(a, b),
                _ => d.warnings.push(format!("{name}: no switch {source} -> {sink}")),
            }
        }
    }
    // Zero-bit switches of the multiplexers nothing configures.
    let mut zero_bit: FxHashMap<&str, Vec<(String, String)>> = FxHashMap::default();
    for tile in &arch.dev.tiles {
        if !zero_bit.contains_key(tile.ttype.as_str()) {
            let bits = arch.db.tile_bits(&tile.ttype)?;
            let arcs = bits
                .muxes
                .values()
                .filter_map(|m| {
                    let (src, _) = m.arcs.iter().find(|(_, b)| b.0.is_empty())?;
                    Some((m.sink.clone(), src.clone()))
                })
                .collect();
            zero_bit.insert(&tile.ttype, arcs);
        }
        let configured = |sink: &str| {
            cc.tiles.get(&tile.name).is_some_and(|c| c.arcs.iter().any(|a| a.0 == sink))
        };
        for (sink, source) in &zero_bit[tile.ttype.as_str()] {
            if configured(sink) {
                continue;
            }
            if let (Some(a), Some(b)) = (
                g.tile_wire(tile.col, tile.row, source, prefix),
                g.tile_wire(tile.col, tile.row, sink, prefix),
            ) {
                d.uf.union(a, b);
            }
        }
    }
    // Fixed connections and the clock network's links; clock buffers.
    for v in 0..n as u32 {
        for e in g.edges(v) {
            let w = g.edge_dst[e];
            if g.edge_tile[e] == NO_TILE && !is_permutation(g.name(v), g.name(w)) {
                d.uf.union(v, w);
            }
        }
        if let Some(rest) = g.name(v).strip_prefix("G_CLKI_")
            && rest.contains("DCC")
            && let Some(out) = g.node(
                g.node_x[v as usize] as usize,
                g.node_y[v as usize] as usize,
                &format!("G_CLKO_{rest}"),
            )
        {
            d.uf.union(v, out);
        }
    }

    // Cells.
    let mut cells = Vec::new();
    for name in cc.tiles.keys() {
        let Some(tile) = arch.dev.tile(name) else { continue };
        if tile.ttype == "PLC2" {
            d.slices(tile, &mut cells)?;
        } else if tile.ttype.starts_with("CIB") || tile.ttype.starts_with("VCIB") {
            d.ties(tile, &mut cells)?;
        }
    }
    for tile in &arch.dev.tiles {
        if let Some(z) = crate::bitgen::ebr_z(&tile.ttype) {
            d.ebr(tile.col, tile.row, z, &mut cells)?;
        }
    }
    let mut ports = Vec::new();
    for (port, (x, y, l)) in pins {
        // The pad and I/O logic tiles both hold the pad's type, which
        // reads back ambiguously (an output's bits match other settings
        // too): either one naming a direction decides.
        let (pio, pic) = crate::bitgen::io_tiles(arch, *x, *y, *l)?;
        let mut types = Vec::new();
        for t in [pio, pic] {
            let tile = arch.dev.tile(&t).ok_or_else(|| format!("no tile {t}"))?;
            types.push(d.enum_value(tile, &format!("PIO{l}.BASE_TYPE"))?);
        }
        let is = |dir: &str| types.iter().any(|t| t.starts_with(dir));
        if is("OUTPUT") {
            let pad = arch.node(*x, *y, &format!("PADDO{l}_PIO"))?;
            ports.push((port.clone(), OUTPUT_PORT, Dir::Input, pad));
        } else if is("INPUT") {
            let pad = arch.node(*x, *y, &format!("JPADDI{l}_PIO"))?;
            ports.push((port.clone(), INPUT_PORT, Dir::Output, pad));
        } else {
            d.warnings.push(format!("port {port}: pad type {} is not decoded", types.join("/")));
        }
    }

    // Nets: one per joined set of wires with a driver.
    let mut nl = Netlist::new();
    nl.top = Some(nl.sym("top"));
    let mut nets: FxHashMap<u32, NetId> = FxHashMap::default();
    let mut drivers: FxHashMap<u32, String> = FxHashMap::default();
    let outputs = cells
        .iter()
        .flat_map(|c| c.outputs.iter().map(move |(p, w)| (format!("{}.{p}", c.name), *w)))
        .chain(ports.iter().filter(|p| p.2 == Dir::Output).map(|p| (p.0.clone(), p.3)));
    for (who, w) in outputs {
        let r = d.uf.find(w);
        if let Some(other) = drivers.get(&r) {
            d.warnings.push(format!("{} drives {other}'s net {}", who, g.full_name(r)));
            continue;
        }
        drivers.insert(r, who);
        nets.insert(r, nl.add_net(&g.full_name(r)));
    }
    let add_pin = |nl: &mut Netlist, c: CellId, pin: &str, dir: Dir, net: Option<NetId>| {
        nl.add_pin(c, pin, dir, net).map(|_| ()).map_err(|e| e.0)
    };
    for c in &cells {
        let id = nl.add_cell(&c.name, c.kind);
        for (k, v) in &c.params {
            nl.set_param(id, k, v.clone());
        }
        for (p, w) in &c.outputs {
            let r = d.uf.find(*w);
            let net = (drivers.get(&r) == Some(&format!("{}.{p}", c.name))).then(|| nets[&r]);
            add_pin(&mut nl, id, p, Dir::Output, net)?;
        }
        for (p, w) in &c.inputs {
            let net = nets.get(&d.uf.find(*w)).copied();
            add_pin(&mut nl, id, p, Dir::Input, net)?;
        }
    }
    for (port, kind, dir, w) in ports {
        let c = nl.add_cell(&port, kind);
        let r = d.uf.find(w);
        let net = match dir {
            Dir::Output => (drivers.get(&r) == Some(&port)).then(|| nets[&r]),
            // An output port reads its pad's net, undriven or not.
            _ => Some(*nets.entry(r).or_insert_with(|| nl.add_net(&g.full_name(r)))),
        };
        add_pin(&mut nl, c, "PAD", dir, net)?;
    }
    Ok((nl, d.warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permutations_are_told_from_wires() {
        assert!(is_permutation("B3", "A3_SLICE"));
        assert!(!is_permutation("A3", "A3_SLICE"));
        assert!(!is_permutation("B3", "A2_SLICE"));
        assert!(!is_permutation("CE0", "CE0_SLICE"));
    }
}
