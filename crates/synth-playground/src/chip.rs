//! The implemented design on the iCE40 die: tiles, placed cells, routed
//! nets with the wires they use, timing, and the configuration bits read
//! back from the bitstream.

use std::collections::{BTreeMap, HashMap};

use arch_ice40::arch::{Arch, BelKind};
use arch_ice40::bitstream::Config;
use arch_ice40::chipdb::{NO_NODE, TileTemplate};
use arch_ice40::device::{TILE_CONFIG_HEIGHT, TileKind};
use arch_ice40::flow::{FlowOptions, Implementation};
use synth_core::Json;
use synth_netlist::{Dir, Param};

/// LUT truth table bit `i` is LC configuration bit `LUT_PERM[i]`.
const LUT_PERM: [usize; 16] = [4, 14, 15, 5, 6, 16, 17, 7, 3, 13, 12, 2, 1, 11, 10, 0];

fn kind_code(k: TileKind) -> u8 {
    match k {
        TileKind::Corner => 0,
        TileKind::Io => 1,
        TileKind::Logic => 2,
        TileKind::RamB => 3,
        TileKind::RamT => 4,
    }
}

fn param(p: Option<&Param>) -> u64 {
    p.and_then(Param::as_u64).unwrap_or(0)
}

/// Readable configuration of a placed cell.
fn cell_info(imp: &Implementation, c: synth_netlist::CellId) -> Vec<Json> {
    let nl = &imp.netlist;
    let p = |n: &str| param(nl.param(c, n));
    let mut out: Vec<(String, String)> = Vec::new();
    match nl.cell_kind(c) {
        "ICESTORM_LC" => {
            let lut_used =
                (0..4).any(|i| nl.pin_net(c, &format!("I{i}")).is_some()) || p("LUT_INIT") != 0;
            if lut_used {
                out.push(("LUT".into(), format!("16'h{:04X}", p("LUT_INIT"))));
            }
            if p("CARRY_ENABLE") != 0 {
                out.push(("carry".into(), "enabled".into()));
            }
            if p("DFF_ENABLE") != 0 {
                let mut ff = String::from("enabled");
                if p("NEG_CLK") != 0 {
                    ff.push_str(", falling edge");
                }
                if nl.pin_net(c, "SR").is_some() {
                    ff.push_str(if p("ASYNC_SR") != 0 { ", async " } else { ", sync " });
                    ff.push_str(if p("SET_NORESET") != 0 { "set" } else { "reset" });
                }
                if nl.pin_net(c, "CEN").is_some() {
                    ff.push_str(", clock enable");
                }
                out.push(("flip-flop".into(), ff));
            }
        }
        "SB_IO" => {
            let t = p("PIN_TYPE");
            out.push(("PIN_TYPE".into(), format!("6'b{t:06b}")));
            let input = match t & 3 {
                0 => "registered",
                1 => "simple",
                2 => "registered latch",
                _ => "latch",
            };
            let output = match t >> 2 {
                0 => "none",
                6 => "simple",
                10 => "tristate",
                _ => "registered / other",
            };
            out.push(("input".into(), input.into()));
            out.push(("output".into(), output.into()));
            if p("PULLUP") != 0 {
                out.push(("pull-up".into(), "on".into()));
            }
        }
        "ICESTORM_RAM" => {
            out.push(("read mode".into(), p("READ_MODE").to_string()));
            out.push(("write mode".into(), p("WRITE_MODE").to_string()));
        }
        _ => {}
    }
    out.into_iter().map(|(a, b)| Json::Arr(vec![a.into(), b.into()])).collect()
}

/// Extents (min/max tile) of routing nodes, by scanning the tile wires.
fn node_extents(arch: &Arch, nodes: &[u32]) -> HashMap<u32, (u8, u8, u8, u8)> {
    let db = arch.db;
    let mut ext: HashMap<u32, (u8, u8, u8, u8)> =
        nodes.iter().map(|&n| (n, (u8::MAX, u8::MAX, 0, 0))).collect();
    let gw = db.grid_width();
    for y in 0..db.grid_height() {
        for x in 0..gw {
            let Some(t) = db.template_at(x, y) else { continue };
            let base = db.tile_wire_base[y * gw + x] as usize;
            for w in 0..t.wires.len() {
                let n = db.tile_wire_nodes[base + w];
                if n == NO_NODE {
                    continue;
                }
                if let Some(e) = ext.get_mut(&n) {
                    e.0 = e.0.min(x as u8);
                    e.1 = e.1.min(y as u8);
                    e.2 = e.2.max(x as u8);
                    e.3 = e.3.max(y as u8);
                }
            }
        }
    }
    ext
}

/// The die, the placed cells and the routed nets.
pub fn chip(imp: &Implementation, opts: &FlowOptions) -> (Json, Json) {
    let arch = &imp.arch;
    let db = arch.db;
    let nl = &imp.netlist;
    let die = db.die;
    let (gw, gh) = (db.grid_width(), db.grid_height());
    let tiles: Vec<u8> =
        (0..gh).flat_map(|y| (0..gw).map(move |x| kind_code(die.tile_kind(x, y)))).collect();
    let pins: Vec<Json> = arch
        .package
        .pins
        .iter()
        .map(|p| Json::Arr(vec![p.name.as_str().into(), p.x.into(), p.y.into(), p.z.into()]))
        .collect();

    // Timing: criticality per net, and the critical paths.
    let (report, tg, ta) =
        arch_ice40::timing::analyse_full(arch, nl, &imp.placement, &imp.routing, &opts.clocks);
    let mut net_crit: HashMap<synth_netlist::NetId, (f64, f64)> = HashMap::new();
    for (k, c) in tg.conns.iter().enumerate() {
        if let Some(n) = nl.cells[c.sink].pins[c.pin].net {
            let e = net_crit.entry(n).or_insert((0.0, f64::INFINITY));
            e.0 = e.0.max(ta.criticality[k]);
            e.1 = e.1.min(ta.slack[k]);
        }
    }

    // Nets, in a stable order, and the cells on their pins.
    let mut nets: Vec<synth_netlist::NetId> = imp.routing.net_edges.keys().copied().collect();
    nets.sort_by_key(|n| nl.str(nl.nets[*n].name).to_string());
    let net_index: HashMap<synth_netlist::NetId, usize> =
        nets.iter().enumerate().map(|(i, n)| (*n, i)).collect();

    let mut cells = Vec::new();
    let mut cell_index: HashMap<synth_netlist::CellId, usize> = HashMap::new();
    let mut placed: Vec<_> = imp.placement.cell_bel.iter().map(|(c, b)| (*c, *b)).collect();
    placed.sort_by_key(|(_, b)| {
        let b = arch.bel(*b);
        (b.x, b.y, b.z, b.kind)
    });
    for (c, b) in placed {
        let bel = arch.bel(b);
        let cell = &nl.cells[c];
        let kind = match bel.kind {
            BelKind::Lc => "lc",
            BelKind::Io => "io",
            BelKind::Ram => "ram",
            BelKind::Gb => "gb",
        };
        let pins: Vec<Json> = cell
            .pins
            .iter()
            .filter_map(|p| {
                let n = p.net?;
                let idx = net_index.get(&n).map(|&i| Json::from(i)).unwrap_or(Json::Null);
                Some(Json::Arr(vec![
                    nl.str(p.name).into(),
                    idx,
                    (p.dir != Dir::Input).into(),
                    nl.str(nl.nets[n].name).into(),
                ]))
            })
            .collect();
        let mut j = Json::obj()
            .with("name", nl.str(cell.name))
            .with("t", nl.cell_kind(c))
            .with("bel", kind)
            .with("x", bel.x)
            .with("y", bel.y)
            .with("z", bel.z)
            .with("info", cell_info(imp, c))
            .with("pins", pins);
        if bel.kind == BelKind::Io
            && let Some(p) = arch.bel_pin_name(b)
        {
            j.set("pin", p);
        }
        if let Some(g) = imp.placement.cell_global.get(&c) {
            j.set("global", *g);
        }
        cell_index.insert(c, cells.len());
        cells.push(j);
    }

    // Routing nodes used, and each net's pips.
    let g = &arch.graph;
    let n_dev = db.nodes.len() as u32;
    let src_of = |e: u32| (g.offsets.partition_point(|&o| o as usize <= e as usize) - 1) as u32;
    let mut used: BTreeMap<u32, usize> = BTreeMap::new();
    let mut net_pips: Vec<Vec<(u32, u32, u16, u16)>> = Vec::new();
    for n in &nets {
        let mut pips = Vec::new();
        for &e in &imp.routing.net_edges[n] {
            let pip = g.edge_pip[e as usize];
            if Arch::permutation(pip).is_some() {
                continue;
            }
            let (a, b) = (src_of(e), g.edge_dst[e as usize]);
            if a >= n_dev || b >= n_dev {
                continue;
            }
            let (px, py) = arch.pip_xy(pip);
            used.entry(a).or_insert(0);
            used.entry(b).or_insert(0);
            pips.push((a, b, px as u16, py as u16));
        }
        net_pips.push(pips);
    }
    let ids: Vec<u32> = used.keys().copied().collect();
    for (i, id) in ids.iter().enumerate() {
        used.insert(*id, i);
    }
    let ext = node_extents(arch, &ids);
    let nodes: Vec<Json> = ids
        .iter()
        .map(|&n| {
            let info = db.nodes[n as usize];
            let (x0, y0, x1, y1) = ext[&n];
            let (x0, y0, x1, y1) =
                if x0 == u8::MAX { (info.x, info.y, info.x, info.y) } else { (x0, y0, x1, y1) };
            Json::Arr(vec![
                db.node_wire_name(synth_core_node(n)).into(),
                format!("{:?}", g.node_class[n as usize]).into(),
                info.x.into(),
                info.y.into(),
                x0.into(),
                y0.into(),
                x1.into(),
                y1.into(),
            ])
        })
        .collect();
    let nets_json: Vec<Json> = nets
        .iter()
        .zip(&net_pips)
        .map(|(n, pips)| {
            let net = &nl.nets[*n];
            let pips: Vec<Json> = pips
                .iter()
                .map(|&(a, b, x, y)| {
                    Json::Arr(vec![used[&a].into(), used[&b].into(), x.into(), y.into()])
                })
                .collect();
            let mut j = Json::obj().with("name", nl.str(net.name)).with("pips", pips);
            if let Some(d) = net.driver
                && let Some(&ci) = cell_index.get(&d.cell)
            {
                j.set("driver", ci);
            }
            let sinks: Vec<usize> =
                net.users.iter().filter_map(|u| cell_index.get(&u.cell).copied()).collect();
            j.set("sinks", sinks);
            if let Some(&(crit, slack)) = net_crit.get(n) {
                j.set("crit", (crit * 1000.0).round() / 1000.0);
                if slack.is_finite() {
                    j.set("slack", (slack / 10.0).round() / 100.0);
                }
            }
            if g.node_class
                .get(imp.routing.net_source.get(n).map_or(0, |s| s.index()))
                .is_some_and(|c| matches!(c, arch_ice40::arch::NodeClass::Global))
                || nl.str(net.name).contains("$glb")
            {
                j.set("global", true);
            }
            j
        })
        .collect();

    let used_lcs = cells.iter().filter(|c| c.at(&["bel"]).as_str() == Some("lc")).count();
    let total_lcs = arch.bels_of(BelKind::Lc).count();
    let chip = Json::obj()
        .with("part", opts.part.name())
        .with("package", arch.package.name.as_str())
        .with("die", die.name())
        .with("gw", gw)
        .with("gh", gh)
        .with("tiles", tiles)
        .with("pins", pins)
        .with("cells", cells)
        .with("nodes", nodes)
        .with("nets", nets_json)
        .with(
            "usage",
            Json::obj()
                .with("lc", used_lcs)
                .with("lc_total", total_lcs)
                .with(
                    "io",
                    imp.placement
                        .cell_bel
                        .values()
                        .filter(|b| arch.bel(**b).kind == BelKind::Io)
                        .count(),
                )
                .with("io_total", arch.package.pins.len())
                .with(
                    "ram",
                    imp.placement
                        .cell_bel
                        .values()
                        .filter(|b| arch.bel(**b).kind == BelKind::Ram)
                        .count(),
                )
                .with("ram_total", arch.bels_of(BelKind::Ram).count()),
        );

    // Clocks and critical paths, located on the die.
    let clocks: Vec<Json> = report
        .clocks
        .iter()
        .enumerate()
        .map(|(ci, c)| {
            let tci = tg.clocks.iter().position(|t| t.name == c.clock).unwrap_or(ci);
            let path: Vec<Json> = ta
                .paths
                .get(tci)
                .and_then(|p| p.as_ref())
                .map(|p| {
                    p.iter()
                        .map(|&(cell, pin, t)| {
                            let mut j = Json::obj()
                                .with("cell", nl.str(nl.cells[cell].name))
                                .with("pin", nl.str(nl.cells[cell].pins[pin].name))
                                .with("t", (t / 10.0).round() / 100.0);
                            if let Some(&i) = cell_index.get(&cell) {
                                j.set("ci", i);
                            }
                            if let Some(n) = nl.cells[cell].pins[pin].net
                                && let Some(&i) = net_index.get(&n)
                            {
                                j.set("net", i);
                            }
                            j
                        })
                        .collect()
                })
                .unwrap_or_default();
            Json::obj()
                .with("clock", c.clock.as_str())
                .with("period_ns", (c.period_ns * 1000.0).round() / 1000.0)
                .with("fmax_mhz", (c.fmax_mhz * 100.0).round() / 100.0)
                .with("target_mhz", c.target_mhz)
                .with("met", c.target_mhz.is_none_or(|t| c.fmax_mhz >= t))
                .with("path", path)
        })
        .collect();
    let timing = Json::obj().with("clocks", clocks);
    (chip, timing)
}

fn synth_core_node(n: u32) -> arch_ice40::chipdb::NodeId {
    arch_ice40::chipdb::NodeId::from_raw(n)
}

/// What every configuration bit of a template does.
struct TemplateDecoder {
    /// (switch, pattern) → source wire.
    pip_src: HashMap<(u16, u16), u16>,
}

impl TemplateDecoder {
    fn new(t: &TileTemplate) -> TemplateDecoder {
        let pip_src = t.pips.iter().map(|p| ((p.switch, p.pattern), p.src)).collect();
        TemplateDecoder { pip_src }
    }
}

/// The configuration image, decoded tile by tile from its bits.
pub fn bitstream(arch: &Arch, cfg: &Config) -> Json {
    let db = arch.db;
    let mut decoders: HashMap<String, TemplateDecoder> = HashMap::new();
    let mut tiles = Vec::new();
    let (mut set_total, mut bits_total) = (0usize, 0usize);
    for (x, y, kind) in cfg.tiles() {
        let w = kind.config_width();
        bits_total += w * TILE_CONFIG_HEIGHT;
        let Some(t) = db.template_at(x, y) else { continue };
        let get = |row: u8, col: u8| cfg.tile_bit(x, y, col as usize, row as usize);
        let mut set = Vec::new();
        for row in 0..TILE_CONFIG_HEIGHT {
            for col in 0..w {
                if cfg.tile_bit(x, y, col, row) {
                    set.push(row * w + col);
                }
            }
        }
        if set.is_empty() {
            continue;
        }
        set_total += set.len();
        let dec = decoders.entry(t.name.clone()).or_insert_with(|| TemplateDecoder::new(t));
        let idx = |b: &arch_ice40::chipdb::TileBit| b.row as usize * w + b.col as usize;
        let mut features = Vec::new();
        for e in &t.config {
            let on: Vec<bool> = e.bits.iter().map(|b| get(b.row, b.col)).collect();
            if !on.iter().any(|&b| b) {
                continue;
            }
            let bits: Vec<usize> = e.bits.iter().filter(|b| get(b.row, b.col)).map(idx).collect();
            let desc = if let Some(z) = e.name.strip_prefix("LC_") {
                let lut: u16 = (0..16).fold(0, |acc, i| acc | (u16::from(on[LUT_PERM[i]]) << i));
                let mut d = format!("logic cell {z}: LUT 16'h{lut:04X}");
                if on[8] {
                    d.push_str(", carry");
                }
                if on[9] {
                    d.push_str(", flip-flop");
                    if on[18] {
                        d.push_str(" (set)");
                    }
                    if on[19] {
                        d.push_str(" (async)");
                    }
                }
                d
            } else {
                e.name.clone()
            };
            features.push(Json::obj().with("n", desc).with("k", "cfg").with("b", bits));
        }
        for (si, sw) in t.switches.iter().enumerate() {
            let mut pattern = 0u16;
            for (k, b) in sw.bits.iter().enumerate() {
                if get(b.row, b.col) {
                    pattern |= 1 << k;
                }
            }
            if pattern == 0 {
                continue;
            }
            let bits: Vec<usize> = sw.bits.iter().filter(|b| get(b.row, b.col)).map(idx).collect();
            let dst = &t.wires[sw.dst as usize];
            let n = match dec.pip_src.get(&(si as u16, pattern)) {
                Some(&src) => format!("{dst} ← {}", t.wires[src as usize]),
                None => format!("{dst} ← pattern {pattern:b}"),
            };
            features.push(
                Json::obj()
                    .with("n", n)
                    .with("k", if sw.routing { "route" } else { "buf" })
                    .with("b", bits),
            );
        }
        tiles.push(
            Json::obj()
                .with("x", x)
                .with("y", y)
                .with("k", kind_code(kind))
                .with("w", w)
                .with("t", t.name.as_str())
                .with("set", set)
                .with("f", features),
        );
    }
    let mut bram = Vec::new();
    for b in arch.bels_of(BelKind::Ram) {
        let bel = arch.bel(b);
        let ones = (0..4096).filter(|&i| cfg.bram_bit(bel.x as usize, bel.y as usize, i)).count();
        if ones > 0 {
            bram.push(Json::obj().with("x", bel.x).with("y", bel.y).with("ones", ones));
        }
    }
    let (cw, ch) = db.die.cram_size();
    Json::obj()
        .with("tiles", tiles)
        .with("bram", bram)
        .with("set_bits", set_total)
        .with("tile_bits", bits_total)
        .with("cram", Json::Arr(vec![cw.into(), ch.into()]))
        .with("bytes", cfg.to_bin().len())
}
