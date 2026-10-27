//! The routing graph of a device, built from its tiles' databases as
//! Project Trellis builds it (`RoutingGraph`, `Chip::get_routing_graph`).
//!
//! A tile's database names wires relative to the tile: `N1_V02S0701` is
//! wire `V02S0701` of the tile one row up, `E3_H06W0003` three columns
//! right; a wire named `G_...` is a global of the clock network, at the
//! origin unless it is a branch or spine (`HPBX`, `VPTX`, `HPRX`), which
//! stay in their tile; names prefixed `25K_`, `45K_` or `85K_` exist only
//! on devices of that size. Every routing multiplexer arc (a configurable
//! switch, owned by the tile whose bits set it) and fixed connection of
//! every tile becomes an edge between the wires it joins. Logic tiles
//! also get, for each LUT input, edges to the other LUT inputs' slice
//! pins: the router may bring a LUT's inputs in any order, and the truth
//! table is permuted to match (nextpnr's LUT permutation).

use synth_core::{FxHashMap, Interner, Sym};

use crate::clocks::{Geometry, Side};
use crate::db::{Database, Device};
use crate::speed::{PipDelay, Speed};

/// No tile: a fixed connection or LUT permutation edge.
pub const NO_TILE: u32 = u32::MAX;

/// A wire named in a tile type's database, relative to the tile.
#[derive(Clone, Copy, Debug)]
enum WireRef {
    /// Not on this device (a size prefix of another device).
    Skip,
    /// `name` at the tile's position plus `(dx, dy)`; `alt` instead from
    /// column 69 on (the transceiver blocks there reuse tile databases with
    /// `PCSA` for `PCSB`).
    At { dx: i16, dy: i16, name: Sym, alt: Sym },
    /// A global of the clock network, at the origin.
    Origin(Sym),
}

/// An arc of a tile type, between wires relative to the tile.
#[derive(Clone, Copy, Debug)]
struct TemplateArc {
    sink: WireRef,
    source: WireRef,
    /// Set by the tile's bits (a multiplexer arc), else always present.
    configurable: bool,
    class: u16,
}

/// The size prefix of names that exist only on this device.
pub fn size_prefix(device: &str) -> &'static str {
    if device.contains("25F") || device.contains("12F") {
        "25K_"
    } else if device.contains("45F") {
        "45K_"
    } else {
        "85K_"
    }
}

/// Resolves a database wire name (Trellis' `globalise_net_ecp5`).
fn wire_ref(db_name: &str, prefix: &str, names: &mut Interner) -> WireRef {
    let mut name = db_name;
    if ["25K_", "45K_", "85K_"].iter().any(|p| db_name.starts_with(p)) {
        if !db_name.starts_with(prefix) {
            return WireRef::Skip;
        }
        name = &db_name[4..];
    }
    let alt_of = |n: &str, names: &mut Interner| match n.find("PCSA") {
        Some(i) => names.intern(&format!("{}PCSB{}", &n[..i], &n[i + 4..])),
        None => names.intern(n),
    };
    if name.starts_with("G_") || name.starts_with("L_") || name.starts_with("R_") {
        let s = names.intern(name);
        if name.starts_with("G_") && !["VPTX", "HPBX", "HPRX"].iter().any(|k| name.contains(k)) {
            return WireRef::Origin(s);
        }
        return WireRef::At { dx: 0, dy: 0, name: s, alt: s };
    }
    // `^([NS]\d+)?([EW]\d+)?_(.*)`.
    let b = name.as_bytes();
    let mut i = 0;
    let (mut dx, mut dy) = (0i16, 0i16);
    let take = |dirs: &[u8], i: &mut usize| -> Option<(u8, i16)> {
        if *i < b.len() && dirs.contains(&b[*i]) {
            let start = *i + 1;
            let mut j = start;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                let n = name[start..j].parse().ok()?;
                let d = b[*i];
                *i = j;
                return Some((d, n));
            }
        }
        None
    };
    let (mut ns, mut ew) = (None, None);
    let mut j = i;
    if let Some(v) = take(b"NS", &mut j) {
        ns = Some(v);
        i = j;
    }
    let mut j = i;
    if let Some(v) = take(b"EW", &mut j) {
        ew = Some(v);
        i = j;
    }
    if i < b.len() && b[i] == b'_' && (ns.is_some() || ew.is_some() || i == 0) {
        for (d, n) in ns.into_iter().chain(ew) {
            match d {
                b'N' => dy -= n,
                b'S' => dy += n,
                b'W' => dx -= n,
                _ => dx += n,
            }
        }
        let rest = &name[i + 1..];
        return WireRef::At { dx, dy, name: names.intern(rest), alt: alt_of(rest, names) };
    }
    WireRef::At { dx: 0, dy: 0, name: names.intern(name), alt: alt_of(name, names) }
}

/// The routing graph of a device, in compressed sparse row form.
#[derive(Debug)]
pub struct Graph {
    pub names: Interner,
    pub node_x: Vec<i16>,
    pub node_y: Vec<i16>,
    pub node_name: Vec<Sym>,
    /// The kind of wire (for the router's lookahead), see [`wire_class`].
    pub node_class: Vec<u16>,
    pub node_cost: Vec<f32>,
    /// Edges leaving node `n` are `offsets[n]..offsets[n + 1]`.
    pub offsets: Vec<u32>,
    pub edge_dst: Vec<u32>,
    pub edge_delay: Vec<f32>,
    /// Index into [`Graph::classes`] of each edge's delay class.
    pub edge_class: Vec<u16>,
    /// The tile whose bits set each edge's switch ([`NO_TILE`] if fixed).
    pub edge_tile: Vec<u32>,
    pub classes: Vec<PipDelay>,
    /// Edges of the clock network's wire links ([`Graph::clock_links`]).
    pub clock_links: usize,
    index: FxHashMap<u64, u32>,
}

/// A wire's key, mixed (Murmur3's finalizer): the map's hasher keeps only
/// low bits of a word's low bits, which the position must reach.
fn key(x: usize, y: usize, name: Sym) -> u64 {
    let mut k = ((x as u64) << 48) | ((y as u64) << 32) | name.index() as u64;
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^ (k >> 33)
}

/// A coarse kind of wire: span wires by length and direction, logic tile
/// pins, clock network and the rest by prefix.
pub fn wire_class(name: &str) -> u16 {
    let b = name.as_bytes();
    if b.len() >= 4
        && (b[0] == b'H' || b[0] == b'V')
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
    {
        let len = (b[1] - b'0') * 10 + (b[2] - b'0');
        let dir = match b[3] {
            b'N' => 0,
            b'S' => 1,
            b'E' => 2,
            b'W' => 3,
            _ => 4,
        };
        return 1 + (len as u16) * 5 + dir;
    }
    let groups = [
        "G_HPBX", "G_VPTX", "G_HPRX", "G_", "L_", "R_", "J", "_SLICE", "F", "Q", "A", "B", "C",
        "D", "M", "CLK", "LSR", "CE", "MUX",
    ];
    for (i, g) in groups.iter().enumerate() {
        let hit = if g.starts_with('_') { name.ends_with(g) } else { name.starts_with(g) };
        if hit {
            return 100 + i as u16;
        }
    }
    99
}

/// Base cost of routing through a wire: longer wires cost more, global
/// clock wires little (they are only reachable from clock buffers).
fn wire_cost(name: &str) -> f32 {
    let b = name.as_bytes();
    if b.len() >= 3
        && (b[0] == b'H' || b[0] == b'V')
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
    {
        return match (b[1] - b'0') * 10 + (b[2] - b'0') {
            0 | 1 => 1.0,
            2 => 1.3,
            _ => 2.0,
        };
    }
    if name.starts_with("G_") { 0.2 } else { 1.0 }
}

impl Graph {
    /// Builds the graph of `dev` with switch delays of `speed`.
    pub fn build(dev: &Device, db: &Database, speed: &Speed) -> Result<Graph, String> {
        let prefix = size_prefix(&dev.info.name);
        let mut names = Interner::new();
        let mut class_index: FxHashMap<String, u16> = FxHashMap::default();
        let mut classes: Vec<PipDelay> = Vec::new();
        // Each tile type's arcs.
        let mut templates: FxHashMap<String, Vec<TemplateArc>> = FxHashMap::default();
        let mut refs: FxHashMap<String, WireRef> = FxHashMap::default();
        for t in &dev.tiles {
            if templates.contains_key(&t.ttype) {
                continue;
            }
            let bits = db.tile_bits(&t.ttype)?;
            let mut arcs = Vec::new();
            let mut resolve = |n: &str, names: &mut Interner| {
                *refs.entry(n.to_string()).or_insert_with(|| wire_ref(n, prefix, names))
            };
            let mut pairs: Vec<(&str, &str, bool)> = Vec::new();
            for mux in bits.muxes.values() {
                for source in mux.arcs.keys() {
                    pairs.push((&mux.sink, source, true));
                }
            }
            for f in &bits.fixed {
                pairs.push((&f.sink, &f.source, false));
            }
            let perm: Vec<(String, String)> = if t.ttype == "PLC2" {
                let abcd = ["A", "B", "C", "D"];
                (0..8)
                    .flat_map(|k| {
                        (0..4).flat_map(move |i| {
                            (0..4).filter(move |&j| j != i).map(move |j| {
                                (format!("{}{k}_SLICE", abcd[i]), format!("{}{k}", abcd[j]))
                            })
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            pairs.extend(perm.iter().map(|(s, d)| (s.as_str(), d.as_str(), false)));
            for (sink, source, configurable) in pairs {
                let (sink_ref, source_ref) =
                    (resolve(sink, &mut names), resolve(source, &mut names));
                if matches!(sink_ref, WireRef::Skip) || matches!(source_ref, WireRef::Skip) {
                    continue;
                }
                // Delays by the wires' positions relative to the tile.
                let at = |r: &WireRef| match *r {
                    WireRef::At { dx, dy, name, .. } => {
                        ((100 + dx) as usize, (100 + dy) as usize, name)
                    }
                    WireRef::Origin(name) => (0, 0, name),
                    WireRef::Skip => unreachable!(),
                };
                let (sx, sy, sn) = at(&source_ref);
                let (dx, dy, dn) = at(&sink_ref);
                let delay = speed.pip(names.resolve(sn), (sx, sy), names.resolve(dn), (dx, dy));
                let ckey = format!("{}:{}", delay.delay, delay.fanout);
                let class = *class_index.entry(ckey).or_insert_with(|| {
                    classes.push(delay);
                    (classes.len() - 1) as u16
                });
                arcs.push(TemplateArc { sink: sink_ref, source: source_ref, configurable, class });
            }
            templates.insert(t.ttype.clone(), arcs);
        }
        let (max_x, max_y) = (dev.info.max_col as i64, dev.info.max_row as i64);
        // A wire: where it is keyed, where it is (a global at the origin
        // is where it is first used), and its name.
        type Placed = ((usize, usize), (usize, usize), Sym);
        let place = |r: WireRef, x: usize, y: usize| -> Option<Placed> {
            match r {
                WireRef::Skip => None,
                WireRef::Origin(name) => Some(((0, 0), (x, y), name)),
                WireRef::At { dx, dy, name, alt } => {
                    let (wx, wy) = (x as i64 + dx as i64, y as i64 + dy as i64);
                    if wx < 0 || wy < 0 || wx > max_x || wy > max_y {
                        return None;
                    }
                    let at = (wx as usize, wy as usize);
                    Some((at, at, if x >= 69 { alt } else { name }))
                }
            }
        };
        let mut g = Graph {
            names,
            node_x: Vec::new(),
            node_y: Vec::new(),
            node_name: Vec::new(),
            node_class: Vec::new(),
            node_cost: Vec::new(),
            offsets: Vec::new(),
            edge_dst: Vec::new(),
            edge_delay: Vec::new(),
            edge_class: Vec::new(),
            edge_tile: Vec::new(),
            classes,
            clock_links: 0,
            index: FxHashMap::default(),
        };
        // Pass 1: nodes, and the edges leaving each.
        let mut degree: Vec<u32> = Vec::new();
        for t in &dev.tiles {
            for a in &templates[&t.ttype] {
                let (Some(s), Some(d)) =
                    (place(a.source, t.col, t.row), place(a.sink, t.col, t.row))
                else {
                    continue;
                };
                let s = g.node_or_insert(s);
                g.node_or_insert(d);
                degree.resize(g.node_x.len(), 0);
                degree[s as usize] += 1;
            }
        }
        degree.resize(g.node_x.len(), 0);
        let implicit = g.clock_links(dev);
        g.clock_links = implicit.len();
        for &(s, _) in &implicit {
            degree[s as usize] += 1;
        }
        // Pass 2: the edges.
        let mut offsets = Vec::with_capacity(degree.len() + 1);
        let mut total = 0u32;
        offsets.push(0);
        for &d in &degree {
            total += d;
            offsets.push(total);
        }
        let n = total as usize;
        let (mut dst, mut class, mut tile) = (vec![0u32; n], vec![0u16; n], vec![NO_TILE; n]);
        // The clock network's links take no time (skew is not modelled).
        let free = g.classes.iter().position(|c| *c == PipDelay::default()).unwrap_or_else(|| {
            g.classes.push(PipDelay::default());
            g.classes.len() - 1
        }) as u16;
        let mut cursor: Vec<u32> = offsets[..degree.len()].to_vec();
        for (ti, t) in dev.tiles.iter().enumerate() {
            for a in &templates[&t.ttype] {
                let (Some(s), Some(d)) =
                    (place(a.source, t.col, t.row), place(a.sink, t.col, t.row))
                else {
                    continue;
                };
                let (s, d) = (g.index[&key(s.0.0, s.0.1, s.2)], g.index[&key(d.0.0, d.0.1, d.2)]);
                let e = cursor[s as usize] as usize;
                cursor[s as usize] += 1;
                dst[e] = d;
                class[e] = a.class;
                tile[e] = if a.configurable { ti as u32 } else { NO_TILE };
            }
        }
        for &(s, d) in &implicit {
            let e = cursor[s as usize] as usize;
            cursor[s as usize] += 1;
            dst[e] = d;
            class[e] = free;
        }
        // One edge per pair of wires, a fixed one where there is one.
        let mut keep = 0usize;
        let mut new_offsets = Vec::with_capacity(offsets.len());
        new_offsets.push(0u32);
        let mut order: Vec<(u32, bool, u32, u16)> = Vec::new();
        for v in 0..degree.len() {
            let (lo, hi) = (offsets[v] as usize, offsets[v + 1] as usize);
            order.clear();
            order.extend((lo..hi).map(|e| (dst[e], tile[e] != NO_TILE, tile[e], class[e])));
            order.sort_unstable();
            order.dedup_by_key(|e| e.0);
            for &(d, _, t, c) in &order {
                dst[keep] = d;
                class[keep] = c;
                tile[keep] = t;
                keep += 1;
            }
            new_offsets.push(keep as u32);
        }
        dst.truncate(keep);
        class.truncate(keep);
        tile.truncate(keep);
        g.offsets = new_offsets;
        g.edge_delay = class.iter().map(|&c| g.classes[c as usize].delay).collect();
        g.edge_dst = dst;
        g.edge_class = class;
        g.edge_tile = tile;
        Ok(g)
    }

    fn node_or_insert(
        &mut self,
        ((kx, ky), (x, y), name): ((usize, usize), (usize, usize), Sym),
    ) -> u32 {
        let k = key(kx, ky, name);
        if let Some(&n) = self.index.get(&k) {
            return n;
        }
        let n = self.node_x.len() as u32;
        let s = self.names.resolve(name);
        self.node_class.push(wire_class(s));
        self.node_cost.push(wire_cost(s));
        self.node_x.push(x as i16);
        self.node_y.push(y as i16);
        self.node_name.push(name);
        self.index.insert(k, n);
        n
    }

    /// The links of the clock network that are wires rather than
    /// switches (see [`crate::clocks`]): quadrant globals to their spines'
    /// rows, spines to their taps, taps to their tiles' branches.
    fn clock_links(&self, dev: &Device) -> Vec<(u32, u32)> {
        let geo = Geometry::from_json(&dev.globals);
        let mut links = Vec::new();
        for q in ["UL", "UR", "LL", "LR"] {
            for n in 0..16 {
                let Some(src) = self.node(0, 0, &format!("G_{q}PCLK{n}")) else { continue };
                for (row, col) in geo.spines_of(q) {
                    if let Some(d) = self.lookup(col, row, &format!("G_HPRX{n:02}00")) {
                        links.push((src, d));
                    }
                }
            }
        }
        for v in 0..self.num_nodes() as u32 {
            let name = self.name(v);
            let (x, y) = (self.node_x[v as usize] as usize, self.node_y[v as usize] as usize);
            if name.starts_with("G_VPTX") {
                let spine = geo.quadrant(y, x).and_then(|q| geo.spine(q, x));
                if let Some((row, col)) = spine
                    && (col, row) != (x, y)
                    && let Some(s) = self.lookup(col, row, name)
                {
                    links.push((s, v));
                }
            } else if let Some(branch) = name.strip_prefix("G_HPBX")
                && let Some((side, tap)) = geo.tap(x)
            {
                let side = if side == Side::Left { "L" } else { "R" };
                if let Some(s) = self.lookup(tap, y, &format!("{side}_HPBX{branch}")) {
                    links.push((s, v));
                }
            }
        }
        links
    }

    /// The wire keyed `name` at exactly `(x, y)`.
    fn lookup(&self, x: usize, y: usize, name: &str) -> Option<u32> {
        let sym = self.names.get(name)?;
        self.index.get(&key(x, y, sym)).copied()
    }

    pub fn num_nodes(&self) -> usize {
        self.node_x.len()
    }

    pub fn num_edges(&self) -> usize {
        self.edge_dst.len()
    }

    /// The wire `name` of the tile at `(x, y)` (a global's position is
    /// ignored where globals sit at the origin).
    pub fn node(&self, x: usize, y: usize, name: &str) -> Option<u32> {
        let sym = self.names.get(name)?;
        self.index.get(&key(x, y, sym)).or_else(|| self.index.get(&key(0, 0, sym))).copied()
    }

    pub fn name(&self, n: u32) -> &str {
        self.names.resolve(self.node_name[n as usize])
    }

    /// `R<y>C<x>_<name>`.
    pub fn full_name(&self, n: u32) -> String {
        format!("R{}C{}_{}", self.node_y[n as usize], self.node_x[n as usize], self.name(n))
    }

    pub fn edges(&self, n: u32) -> std::ops::Range<usize> {
        self.offsets[n as usize] as usize..self.offsets[n as usize + 1] as usize
    }

    /// The router's view.
    pub fn route_graph(&self) -> synth_route::Graph<'_> {
        synth_route::Graph {
            offsets: &self.offsets,
            edge_dst: &self.edge_dst,
            edge_delay: &self.edge_delay,
            node_x: &self.node_x,
            node_y: &self.node_y,
            node_cost: &self.node_cost,
        }
    }

    /// A wire's name as the database of the tile at `(x, y)` names it:
    /// relative to the tile, or a global's own name (Trellis' wire naming,
    /// as nextpnr writes switches).
    pub fn name_in_tile(&self, n: u32, x: usize, y: usize) -> String {
        let base = self.name(n);
        if base.starts_with("G_") || base.starts_with("L_") || base.starts_with("R_") {
            return base.to_string();
        }
        let (wx, wy) = (self.node_x[n as usize] as i64, self.node_y[n as usize] as i64);
        let (x, y) = (x as i64, y as i64);
        if (wx, wy) == (x, y) {
            return base.to_string();
        }
        let mut rel = String::new();
        if wy < y {
            rel += &format!("N{}", y - wy);
        }
        if wy > y {
            rel += &format!("S{}", wy - y);
        }
        if wx > x {
            rel += &format!("E{}", wx - x);
        }
        if wx < x {
            rel += &format!("W{}", x - wx);
        }
        format!("{rel}_{base}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_names_resolve_relative_to_their_tile() {
        let mut names = Interner::new();
        let mut at = |s: &str, p: &str| match wire_ref(s, p, &mut names) {
            WireRef::At { dx, dy, name, .. } => Some((dx, dy, names.resolve(name).to_string())),
            WireRef::Origin(name) => Some((99, 99, names.resolve(name).to_string())),
            WireRef::Skip => None,
        };
        assert_eq!(at("N1_V02S0701", "25K_"), Some((0, -1, "V02S0701".into())));
        assert_eq!(at("E3_H06W0003", "25K_"), Some((3, 0, "H06W0003".into())));
        assert_eq!(at("S1W2_JECLK0", "25K_"), Some((-2, 1, "JECLK0".into())));
        assert_eq!(at("F5A_SLICE", "25K_"), Some((0, 0, "F5A_SLICE".into())));
        assert_eq!(at("H01E0001", "25K_"), Some((0, 0, "H01E0001".into())));
        assert_eq!(at("G_HPBX0100", "25K_"), Some((0, 0, "G_HPBX0100".into())));
        assert_eq!(at("G_CLKI_LDCC0", "25K_"), Some((99, 99, "G_CLKI_LDCC0".into())));
        assert_eq!(at("45K_N1W6_JD7", "25K_"), None);
        assert_eq!(at("25K_N1W6_JD7", "25K_"), Some((-6, -1, "JD7".into())));
    }
}
