//! Conversion of IceStorm's text chip database (`icebox_chipdb` output) and
//! timing files into a [`ChipDb`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use synth_core::FxHashMap;

use super::*;
use crate::device::Die;

/// An error while importing IceStorm data.
#[derive(Clone, Debug)]
pub struct ImportError(pub String);

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "chip database import: {}", self.0)
    }
}

impl std::error::Error for ImportError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ImportError> {
    Err(ImportError(msg.into()))
}

fn num<T: std::str::FromStr>(s: Option<&str>, what: &str, line: usize) -> Result<T, ImportError> {
    s.and_then(|s| s.parse().ok())
        .ok_or_else(|| ImportError(format!("line {line}: expected {what}")))
}

fn parse_bit(s: &str) -> Result<TileBit, ImportError> {
    // B<row>[<col>]
    let inner = s.strip_prefix('B').and_then(|s| s.strip_suffix(']'));
    let parsed = inner
        .and_then(|s| s.split_once('['))
        .and_then(|(r, c)| Some(TileBit { row: r.parse().ok()?, col: c.parse().ok()? }));
    parsed.ok_or_else(|| ImportError(format!("bad config bit `{s}`")))
}

struct RawSwitch {
    x: u8,
    y: u8,
    dst: u32,
    routing: bool,
    bits: Vec<TileBit>,
    sources: Vec<(u16, u32)>,
}

#[derive(Default)]
struct Raw {
    die: Option<Die>,
    num_nets: usize,
    tiles: BTreeMap<(u8, u8), TileKind>,
    tile_bits: BTreeMap<TileKind, Vec<ConfigEntry>>,
    nets: Vec<Vec<(u8, u8, String)>>,
    switches: Vec<RawSwitch>,
    packages: Vec<Package>,
    gbufin: Vec<(u8, u8, u8)>,
    gbufpin: Vec<(u8, u8, u8, u8)>,
    iolatch: Vec<(u8, u8)>,
    ieren: Vec<IeRen>,
    extra_cells: Vec<ExtraCell>,
    extra_bits: Vec<(String, u8, u16, u16)>,
}

fn parse_chipdb(text: &str) -> Result<Raw, ImportError> {
    enum Mode {
        None,
        Net(usize),
        Switch,
        Bits(TileKind),
        Pins,
        GbufIn,
        GbufPin,
        IoLatch,
        IeRen,
        ExtraCell,
        ExtraBits,
        Skip,
    }
    let mut raw = Raw::default();
    let mut mode = Mode::None;
    for (i, line) in text.lines().enumerate() {
        let ln = i + 1;
        let mut t = line.split_whitespace();
        let Some(first) = t.next() else { continue };
        if first.starts_with('#') {
            continue;
        }
        if first.starts_with('.') {
            mode = Mode::None;
            match first {
                ".device" => {
                    let name: &str = t.next().unwrap_or("");
                    raw.die = Some(name.parse().map_err(ImportError)?);
                    let _w: usize = num(t.next(), "width", ln)?;
                    let _h: usize = num(t.next(), "height", ln)?;
                    raw.num_nets = num(t.next(), "net count", ln)?;
                    raw.nets = vec![Vec::new(); raw.num_nets];
                }
                ".io_tile" | ".logic_tile" | ".ramb_tile" | ".ramt_tile" => {
                    let kind = match first {
                        ".io_tile" => TileKind::Io,
                        ".logic_tile" => TileKind::Logic,
                        ".ramb_tile" => TileKind::RamB,
                        _ => TileKind::RamT,
                    };
                    raw.tiles.insert((num(t.next(), "x", ln)?, num(t.next(), "y", ln)?), kind);
                }
                ".io_tile_bits" | ".logic_tile_bits" | ".ramb_tile_bits" | ".ramt_tile_bits" => {
                    let kind = match first {
                        ".io_tile_bits" => TileKind::Io,
                        ".logic_tile_bits" => TileKind::Logic,
                        ".ramb_tile_bits" => TileKind::RamB,
                        _ => TileKind::RamT,
                    };
                    mode = Mode::Bits(kind);
                }
                ".net" => {
                    let id: usize = num(t.next(), "net id", ln)?;
                    if id >= raw.nets.len() {
                        return err(format!("line {ln}: net {id} out of range"));
                    }
                    mode = Mode::Net(id);
                }
                ".buffer" | ".routing" => {
                    let x = num(t.next(), "x", ln)?;
                    let y = num(t.next(), "y", ln)?;
                    let dst = num(t.next(), "net", ln)?;
                    let bits = t.map(parse_bit).collect::<Result<Vec<_>, _>>()?;
                    raw.switches.push(RawSwitch {
                        x,
                        y,
                        dst,
                        routing: first == ".routing",
                        bits,
                        sources: Vec::new(),
                    });
                    mode = Mode::Switch;
                }
                ".pins" => {
                    raw.packages
                        .push(Package { name: t.next().unwrap_or("").into(), pins: vec![] });
                    mode = Mode::Pins;
                }
                ".gbufin" => mode = Mode::GbufIn,
                ".gbufpin" => mode = Mode::GbufPin,
                ".iolatch" => mode = Mode::IoLatch,
                ".ieren" => mode = Mode::IeRen,
                ".extra_cell" => {
                    let x = num(t.next(), "x", ln)?;
                    let y = num(t.next(), "y", ln)?;
                    let kind = t.next().unwrap_or("").to_string();
                    raw.extra_cells.push(ExtraCell { kind, x, y, locked: vec![], entries: vec![] });
                    mode = Mode::ExtraCell;
                }
                ".extra_bits" => mode = Mode::ExtraBits,
                // Column buffer topology is fixed per die; not needed.
                ".colbuf" => mode = Mode::Skip,
                other => return err(format!("line {ln}: unknown section {other}")),
            }
            continue;
        }
        match mode {
            Mode::None => return err(format!("line {ln}: data outside a section")),
            Mode::Skip => {}
            Mode::Net(id) => {
                let x = first.parse().map_err(|_| ImportError(format!("line {ln}: bad x")))?;
                let y = num(t.next(), "y", ln)?;
                let name = t.next().ok_or_else(|| ImportError(format!("line {ln}: no name")))?;
                raw.nets[id].push((x, y, name.to_string()));
            }
            Mode::Switch => {
                let sw = raw.switches.last_mut().unwrap();
                let pattern = u16::from_str_radix(first, 2)
                    .map_err(|_| ImportError(format!("line {ln}: bad pattern")))?;
                if first.len() != sw.bits.len() {
                    return err(format!("line {ln}: pattern width mismatch"));
                }
                // Reverse so that bit k of the stored pattern belongs to bits[k].
                let mut p = 0u16;
                for k in 0..sw.bits.len() {
                    if pattern >> (sw.bits.len() - 1 - k) & 1 != 0 {
                        p |= 1 << k;
                    }
                }
                sw.sources.push((p, num(t.next(), "net", ln)?));
            }
            Mode::Bits(kind) => {
                let bits = t.map(parse_bit).collect::<Result<Vec<_>, _>>()?;
                raw.tile_bits
                    .entry(kind)
                    .or_default()
                    .push(ConfigEntry { name: first.into(), bits });
            }
            Mode::Pins => {
                let pin = PackagePin {
                    name: first.into(),
                    x: num(t.next(), "x", ln)?,
                    y: num(t.next(), "y", ln)?,
                    z: num(t.next(), "z", ln)?,
                };
                raw.packages.last_mut().unwrap().pins.push(pin);
            }
            Mode::GbufIn => {
                let x = first.parse().map_err(|_| ImportError(format!("line {ln}: bad x")))?;
                raw.gbufin.push((x, num(t.next(), "y", ln)?, num(t.next(), "glb", ln)?));
            }
            Mode::GbufPin => {
                let x = first.parse().map_err(|_| ImportError(format!("line {ln}: bad x")))?;
                raw.gbufpin.push((
                    x,
                    num(t.next(), "y", ln)?,
                    num(t.next(), "z", ln)?,
                    num(t.next(), "glb", ln)?,
                ));
            }
            Mode::IoLatch => {
                let x = first.parse().map_err(|_| ImportError(format!("line {ln}: bad x")))?;
                raw.iolatch.push((x, num(t.next(), "y", ln)?));
            }
            Mode::IeRen => {
                let x = first.parse().map_err(|_| ImportError(format!("line {ln}: bad x")))?;
                raw.ieren.push(IeRen {
                    io: (x, num(t.next(), "y", ln)?, num(t.next(), "z", ln)?),
                    ieren: (
                        num(t.next(), "x", ln)?,
                        num(t.next(), "y", ln)?,
                        num(t.next(), "z", ln)?,
                    ),
                });
            }
            Mode::ExtraCell => {
                let cell = raw.extra_cells.last_mut().unwrap();
                if first == "LOCKED" {
                    cell.locked.extend(t.map(String::from));
                } else {
                    cell.entries.push(ExtraCellEntry {
                        key: first.into(),
                        x: num(t.next(), "x", ln)?,
                        y: num(t.next(), "y", ln)?,
                        name: t.next().unwrap_or("").into(),
                    });
                }
            }
            Mode::ExtraBits => {
                raw.extra_bits.push((
                    first.into(),
                    num(t.next(), "bank", ln)?,
                    num(t.next(), "x", ln)?,
                    num(t.next(), "y", ln)?,
                ));
            }
        }
    }
    if raw.die.is_none() {
        return err("missing .device");
    }
    Ok(raw)
}

fn parse_timings(text: &str) -> Result<Vec<TimingCell>, ImportError> {
    let mut cells: Vec<TimingCell> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.is_empty() {
            continue;
        }
        if t[0] == "CELL" {
            cells.push(TimingCell { name: t.get(1).copied().unwrap_or("").into(), arcs: vec![] });
            continue;
        }
        let kind = match t[0] {
            "IOPATH" => ArcKind::IoPath,
            "SETUP" => ArcKind::Setup,
            "HOLD" => ArcKind::Hold,
            "RECOVERY" => ArcKind::Recovery,
            "REMOVAL" => ArcKind::Removal,
            _ => return err(format!("timing line {}: unknown record {}", i + 1, t[0])),
        };
        // min:typ:max triples; take the max. `*` means no value.
        let max = |s: Option<&&str>| -> f32 {
            s.and_then(|s| s.split(':').nth(2)).and_then(|v| v.parse().ok()).unwrap_or(0.0)
        };
        let Some(cell) = cells.last_mut() else {
            return err(format!("timing line {}: record before CELL", i + 1));
        };
        let rise = max(t.get(3));
        let fall = if kind == ArcKind::IoPath { max(t.get(4)) } else { rise };
        cell.arcs.push(TimingArc {
            kind,
            from: t.get(1).copied().unwrap_or("").into(),
            to: t.get(2).copied().unwrap_or("").into(),
            rise,
            fall,
        });
    }
    Ok(cells)
}

/// Template name for an IO tile at `(x, y)` (one template per chip edge),
/// or the tile kind's name otherwise.
fn template_name(die: Die, x: usize, y: usize, kind: TileKind) -> &'static str {
    match kind {
        TileKind::Io if x == 0 => "io_l",
        TileKind::Io if x == die.grid_width() - 1 => "io_r",
        TileKind::Io if y == 0 => "io_b",
        TileKind::Io => "io_t",
        other => other.name(),
    }
}

/// Wires that name the main driver of a net; preferred as canonical names.
fn is_major_wire(name: &str) -> bool {
    name.starts_with("lutff_")
        || name.starts_with("io_")
        || name.starts_with("ram/")
        || name.starts_with("glb_netwk_")
}

/// Imports an IceStorm chip database and timing file.
pub fn import(chipdb_text: &str, timing_text: &str) -> Result<ChipDb, ImportError> {
    let raw = parse_chipdb(chipdb_text)?;
    let die = raw.die.unwrap();
    let (gw, gh) = (die.grid_width(), die.grid_height());

    for (&(x, y), &kind) in &raw.tiles {
        if die.tile_kind(x as usize, y as usize) != kind {
            return err(format!("tile ({x},{y}) kind mismatch"));
        }
    }

    // Template of every tile.
    let mut template_names: Vec<&'static str> = Vec::new();
    let mut tile_template = vec![u8::MAX; gw * gh];
    for y in 0..gh {
        for x in 0..gw {
            let kind = die.tile_kind(x, y);
            if kind == TileKind::Corner {
                continue;
            }
            let name = template_name(die, x, y, kind);
            let idx = match template_names.iter().position(|n| *n == name) {
                Some(i) => i,
                None => {
                    template_names.push(name);
                    template_names.len() - 1
                }
            };
            tile_template[y * gw + x] = idx as u8;
        }
    }

    // Local names of each net in each tile.
    let mut tile_net_names: FxHashMap<(u8, u8, u32), Vec<&str>> = FxHashMap::default();
    let mut template_wires: Vec<BTreeSet<&str>> = vec![BTreeSet::new(); template_names.len()];
    for (net, entries) in raw.nets.iter().enumerate() {
        for (x, y, name) in entries {
            let t = tile_template[*y as usize * gw + *x as usize];
            if t == u8::MAX {
                return err(format!("net {net} touches corner tile"));
            }
            template_wires[t as usize].insert(name);
            tile_net_names.entry((*x, *y, net as u32)).or_default().push(name);
        }
    }
    let wire_lists: Vec<Vec<&str>> =
        template_wires.iter().map(|s| s.iter().copied().collect()).collect();
    let wire_maps: Vec<FxHashMap<&str, u16>> = wire_lists
        .iter()
        .map(|l| l.iter().enumerate().map(|(i, &w)| (w, i as u16)).collect())
        .collect();

    // Switch templates: group by (bits, routing); resolve local names by
    // intersecting the candidate names across all tiles of the template.
    struct Group {
        dst: BTreeSet<String>,
        sources: BTreeMap<u16, BTreeSet<String>>,
    }
    let mut groups: Vec<BTreeMap<(Vec<TileBit>, bool), Group>> =
        (0..template_names.len()).map(|_| BTreeMap::new()).collect();
    let names_of = |x: u8, y: u8, net: u32| -> Result<BTreeSet<String>, ImportError> {
        let names = tile_net_names
            .get(&(x, y, net))
            .ok_or_else(|| ImportError(format!("net {net} not present in tile ({x},{y})")))?;
        Ok(names.iter().map(|s| s.to_string()).collect())
    };
    for sw in &raw.switches {
        let t = tile_template[sw.y as usize * gw + sw.x as usize] as usize;
        let dst_names = names_of(sw.x, sw.y, sw.dst)?;
        let g = groups[t]
            .entry((sw.bits.clone(), sw.routing))
            .or_insert_with(|| Group { dst: dst_names.clone(), sources: BTreeMap::new() });
        g.dst = g.dst.intersection(&dst_names).cloned().collect();
        for &(pattern, src) in &sw.sources {
            let names = names_of(sw.x, sw.y, src)?;
            match g.sources.get_mut(&pattern) {
                Some(s) => *s = s.intersection(&names).cloned().collect(),
                None => {
                    g.sources.insert(pattern, names);
                }
            }
        }
    }

    let mut templates = Vec::new();
    for (t, name) in template_names.iter().enumerate() {
        let kind = match *name {
            "logic" => TileKind::Logic,
            "ramb" => TileKind::RamB,
            "ramt" => TileKind::RamT,
            _ => TileKind::Io,
        };
        let mut switches = Vec::new();
        let mut pips = Vec::new();
        for ((bits, routing), g) in &groups[t] {
            let pick = |set: &BTreeSet<String>, what: &str| -> Result<u16, ImportError> {
                let first = set
                    .iter()
                    .next()
                    .ok_or_else(|| ImportError(format!("{name}: ambiguous {what} wire")))?;
                Ok(wire_maps[t][first.as_str()])
            };
            let dst = pick(&g.dst, "switch destination")?;
            let switch = switches.len() as u16;
            switches.push(Switch { dst, routing: *routing, bits: bits.clone() });
            for (&pattern, srcs) in &g.sources {
                pips.push(TemplatePip { switch, src: pick(srcs, "pip source")?, pattern });
            }
        }
        templates.push(TileTemplate {
            name: name.to_string(),
            kind,
            wires: wire_lists[t].iter().map(|s| s.to_string()).collect(),
            config: raw.tile_bits.get(&kind).cloned().unwrap_or_default(),
            switches,
            pips,
        });
    }

    // Per-tile local wire -> node table.
    let mut tile_wire_base = vec![0u32; gw * gh];
    let mut tile_wire_nodes = Vec::new();
    for i in 0..gw * gh {
        tile_wire_base[i] = tile_wire_nodes.len() as u32;
        if tile_template[i] != u8::MAX {
            let n = templates[tile_template[i] as usize].wires.len();
            tile_wire_nodes.extend(std::iter::repeat_n(NO_NODE, n));
        }
    }
    let mut nodes = Vec::with_capacity(raw.nets.len());
    for (net, entries) in raw.nets.iter().enumerate() {
        if entries.is_empty() {
            return err(format!("net {net} has no wires"));
        }
        let mut canon: Option<NodeInfo> = None;
        let mut canon_major = false;
        for (x, y, name) in entries {
            let i = *y as usize * gw + *x as usize;
            let t = tile_template[i] as usize;
            let w = wire_maps[t][name.as_str()];
            tile_wire_nodes[tile_wire_base[i] as usize + w as usize] = net as u32;
            let major = is_major_wire(name);
            if canon.is_none() || (major && !canon_major) {
                canon = Some(NodeInfo { x: *x, y: *y, wire: w });
                canon_major = major;
            }
        }
        nodes.push(canon.unwrap());
    }

    let db_partial = ChipDb {
        die,
        templates,
        tile_template,
        tile_wire_base,
        tile_wire_nodes,
        nodes,
        packages: raw.packages,
        globals: [GlobalNetwork::default(); 8],
        iolatch: raw.iolatch,
        ieren: raw.ieren,
        extra_cells: raw.extra_cells,
        timing: parse_timings(timing_text)?,
    };
    let mut db = db_partial;

    // Global networks.
    for &(x, y, g) in &raw.gbufin {
        db.globals.get_mut(g as usize).ok_or(ImportError("bad global index".into()))?.fabout =
            (x, y);
    }
    for &(x, y, z, g) in &raw.gbufpin {
        db.globals.get_mut(g as usize).ok_or(ImportError("bad global index".into()))?.pad =
            (x, y, z);
    }
    for (name, bank, x, y) in &raw.extra_bits {
        if let Some(g) = name.strip_prefix("padin_glb_netwk.") {
            let g: usize = g.parse().map_err(|_| ImportError(format!("bad extra bit {name}")))?;
            db.globals.get_mut(g).ok_or(ImportError("bad global index".into()))?.pad_extra_bit =
                (*bank, *x, *y);
        }
    }

    verify_against_raw(&db, &raw.switches)?;
    Ok(db)
}

/// Checks that the templated routing reproduces exactly the switches listed
/// for every tile in the source data: no pip lost, none invented.
fn verify_against_raw(db: &ChipDb, switches: &[RawSwitch]) -> Result<(), ImportError> {
    let mut expected: BTreeSet<(u8, u8, u32, u32, Vec<TileBit>, u16)> = BTreeSet::new();
    for sw in switches {
        for &(pattern, src) in &sw.sources {
            expected.insert((sw.x, sw.y, src, sw.dst, sw.bits.clone(), pattern));
        }
    }
    let mut found = BTreeSet::new();
    for y in 0..db.grid_height() {
        for x in 0..db.grid_width() {
            let Some(t) = db.template_at(x, y) else { continue };
            for p in &t.pips {
                let s = &t.switches[p.switch as usize];
                let (Some(src), Some(dst)) =
                    (db.tile_wire_node(x, y, p.src), db.tile_wire_node(x, y, s.dst))
                else {
                    continue;
                };
                found.insert((x as u8, y as u8, src.raw(), dst.raw(), s.bits.clone(), p.pattern));
            }
        }
    }
    if found != expected {
        let missing = expected.difference(&found).count();
        let extra = found.difference(&expected).count();
        let example = found.difference(&expected).next().or(expected.difference(&found).next());
        return err(format!(
            "templated routing differs from source: {missing} missing, {extra} invented pips \
             (e.g. tile ({:?}))",
            example.map(|e| (e.0, e.1, e.2, e.3))
        ));
    }
    Ok(())
}
