//! Native reader for Project X-Ray's tile grid, connectivity and part geometry.
//! Routing wires joined by tileconn are collapsed once, before CSR construction.
use crate::json::{self, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use synth_core::FxHashMap;

pub(crate) fn read_json(p: &Path) -> Result<Value, String> {
    json::parse(&synth_core::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?)
        .map_err(|e| format!("{}: {e}", p.display()))
}
fn u32_field(v: &Value, key: &str) -> Result<u32, String> {
    u32::try_from(v.get(key)?.integer()?).map_err(|_| format!("{key} out of range"))
}

#[derive(Clone, Debug)]
pub struct Part {
    pub name: String,
    pub idcode: u32,
    pub frames: Vec<u32>,
}
impl Part {
    pub fn load(root: &Path, name: &str) -> Result<Self, String> {
        if !name.starts_with("xc7a")
            || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err(format!("invalid Artix-7 part {name}"));
        }
        let j = read_json(&root.join(name).join("part.json"))?;
        let mut frames = Vec::new();
        for (half, region) in j.get("global_clock_regions")?.object()? {
            let bottom = match half.as_str() {
                "top" => 0,
                "bottom" => 1,
                _ => return Err(format!("unknown half {half}")),
            };
            for (row, data) in region.get("rows")?.object()? {
                let row: u32 = row.parse().map_err(|_| "invalid row")?;
                if row >= 32 {
                    return Err("frame row exceeds address field".into());
                }
                for (bus, data) in data.get("configuration_buses")?.object()? {
                    let bus = match bus.as_str() {
                        "CLB_IO_CLK" => 0,
                        "BLOCK_RAM" => 1,
                        "CFG_CLB" => 2,
                        _ => return Err(format!("unknown bus {bus}")),
                    };
                    for (col, data) in data.get("configuration_columns")?.object()? {
                        let col: u32 = col.parse().map_err(|_| "invalid column")?;
                        let count = u32_field(data, "frame_count")?;
                        if col >= 1024 || count > 128 || count == 0 {
                            return Err("invalid frame column".into());
                        }
                        frames.extend((0..count).map(|minor| {
                            (bus << 23) | (bottom << 22) | (row << 17) | (col << 7) | minor
                        }));
                    }
                }
            }
        }
        frames.sort_unstable();
        if frames.is_empty() {
            return Err("part has no frames".into());
        }
        Ok(Self { name: name.into(), idcode: u32_field(&j, "idcode")?, frames })
    }
    pub fn fabric(&self) -> Result<&'static str, String> {
        if self.name.starts_with("xc7a35t") || self.name.starts_with("xc7a50t") {
            Ok("xc7a50t")
        } else if self.name.starts_with("xc7a100t") {
            Ok("xc7a100t")
        } else if self.name.starts_with("xc7a200t") {
            Ok("xc7a200t")
        } else {
            Err(format!("unknown fabric for {}", self.name))
        }
    }
}

#[derive(Clone, Debug)]
pub struct BitRegion {
    pub base: u32,
    pub frames: u32,
    pub offset: u32,
    pub words: u32,
    pub alias: Option<(String, u32, BTreeMap<String, String>)>,
}
#[derive(Clone, Debug)]
pub struct Tile {
    pub name: String,
    pub kind: String,
    pub x: i16,
    pub y: i16,
    pub sites: BTreeMap<String, String>,
    pub prohibited: BTreeSet<String>,
    pub bits: BTreeMap<String, BitRegion>,
    pub nodes: Vec<u32>,
    pub template: usize,
}
#[derive(Clone, Debug)]
pub struct SiteTemplate {
    pub name: String,
    pub prefix: String,
    pub kind: String,
    pub pins: BTreeMap<String, u32>,
}
#[derive(Clone, Debug)]
pub struct Pip {
    pub src: u32,
    pub dst: u32,
    pub feature: String,
    /// Worst slow-corner delay in ps, None when upstream has no timing.
    pub delay: Option<f32>,
    /// Drive resistance (ohm) and input capacitance (fF) of the pip, and
    /// whether it buffers (otherwise a pass transistor).
    pub res: f32,
    pub cap: f32,
    pub buffered: bool,
    pub fixed: bool,
    pub config: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct Template {
    pub kind: String,
    pub wires: Vec<String>,
    /// Resistance (ohm) and capacitance (fF) of each wire, where known.
    pub wire_rc: Vec<(f32, f32)>,
    pub sites: Vec<SiteTemplate>,
    pub pips: Vec<Pip>,
}
#[derive(Clone, Debug)]
pub struct PackagePin {
    pub site: String,
    pub tile: String,
    pub bank: u32,
    pub function: String,
}
#[derive(Clone, Debug)]
pub struct Device {
    pub part: Part,
    pub tiles: Vec<Tile>,
    pub templates: Vec<Template>,
    pub pins: BTreeMap<String, PackagePin>,
    pub node_x: Vec<i16>,
    pub node_y: Vec<i16>,
    pub tile_by_name: BTreeMap<String, usize>,
}

impl Device {
    /// Load configuration geometry only, without allocating the routing graph.
    pub fn geometry(root: &Path, name: &str) -> Result<Self, String> {
        let part = Part::load(root, name)?;
        let grid = read_json(&root.join(part.fabric()?).join("tilegrid.json"))?;
        let mut tiles = Vec::new();
        for (name, t) in grid.object()? {
            let mut bits = BTreeMap::new();
            for (bus, b) in t.get("bits")?.object()? {
                let base =
                    u32::from_str_radix(b.get("baseaddr")?.string()?.trim_start_matches("0x"), 16)
                        .map_err(|_| "invalid frame base")?;
                let alias = if let Some(a) = b.object()?.get("alias") {
                    Some((
                        a.get("type")?.string()?.into(),
                        u32_field(a, "start_offset")?,
                        a.get("sites")?
                            .object()?
                            .iter()
                            .map(|(k, v)| Ok((k.clone(), v.string()?.into())))
                            .collect::<Result<_, String>>()?,
                    ))
                } else {
                    None
                };
                let region = BitRegion {
                    base,
                    frames: u32_field(b, "frames")?,
                    offset: u32_field(b, "offset")?,
                    words: u32_field(b, "words")?,
                    alias,
                };
                if region.offset.checked_add(region.words).is_none_or(|n| n > 101)
                    || region.frames > 128
                    || region.base.checked_add(region.frames).is_none()
                {
                    return Err(format!("invalid frame extent for {name}"));
                }
                bits.insert(bus.clone(), region);
            }
            tiles.push(Tile {
                name: name.clone(),
                kind: t.get("type")?.string()?.into(),
                x: i16::try_from(t.get("grid_x")?.integer()?).map_err(|_| "grid x overflow")?,
                y: i16::try_from(t.get("grid_y")?.integer()?).map_err(|_| "grid y overflow")?,
                sites: t
                    .get("sites")?
                    .object()?
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), v.string()?.into())))
                    .collect::<Result<_, String>>()?,
                prohibited: t
                    .get("prohibited_sites")?
                    .array()?
                    .iter()
                    .map(|v| v.string().map(String::from))
                    .collect::<Result<_, _>>()?,
                bits,
                nodes: Vec::new(),
                template: 0,
            });
        }
        let mut pins = BTreeMap::new();
        let csv = synth_core::fs::read_to_string(&root.join(&part.name).join("package_pins.csv"))
            .map_err(|e| e.to_string())?;
        for line in csv.lines().skip(1) {
            let cols: Vec<_> = line.split(',').collect();
            if cols.len() != 5 {
                return Err(format!("malformed package pin: {line}"));
            }
            pins.insert(
                cols[0].into(),
                PackagePin {
                    bank: cols[1].parse().map_err(|_| "invalid IO bank")?,
                    site: cols[2].into(),
                    tile: cols[3].into(),
                    function: cols[4].into(),
                },
            );
        }
        let tile_by_name = tiles.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();
        Ok(Self {
            part,
            tiles,
            templates: Vec::new(),
            pins,
            node_x: Vec::new(),
            node_y: Vec::new(),
            tile_by_name,
        })
    }
    pub fn load(root: &Path, name: &str) -> Result<Self, String> {
        let mut db = Self::geometry(root, name)?;
        let kinds: BTreeSet<_> = db.tiles.iter().map(|t| t.kind.clone()).collect();
        for kind in &kinds {
            db.templates.push(Template::load(root, kind)?);
        }
        let types: BTreeMap<_, _> =
            kinds.iter().enumerate().map(|(i, k)| (k.as_str(), i)).collect();
        let mut total = 0u32;
        for t in &mut db.tiles {
            t.template = types[t.kind.as_str()];
            let len = db.templates[t.template].wires.len() as u32;
            let end = total.checked_add(len).ok_or("too many routing wires")?;
            t.nodes = (total..end).collect();
            total = end;
        }
        let mut parent: Vec<u32> = (0..total).collect();
        let grid: FxHashMap<_, _> =
            db.tiles.iter().enumerate().map(|(i, t)| ((t.x, t.y), i)).collect();
        let conn = read_json(&root.join(db.part.fabric()?).join("tileconn.json"))?;
        let by_type: Vec<Vec<usize>> = (0..db.templates.len())
            .map(|kind| {
                db.tiles
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.template == kind)
                    .map(|(i, _)| i)
                    .collect()
            })
            .collect();
        for c in conn.array()? {
            let ts = c.get("tile_types")?.array()?;
            let delta = c.get("grid_deltas")?.array()?;
            if ts.len() != 2 || delta.len() != 2 {
                return Err("invalid tileconn record".into());
            }
            let (Some(&a), Some(&b)) = (types.get(ts[0].string()?), types.get(ts[1].string()?))
            else {
                continue;
            };
            let (dx, dy) = (delta[0].integer()?, delta[1].integer()?);
            let pairs = c
                .get("wire_pairs")?
                .array()?
                .iter()
                .map(|p| {
                    let p = p.array()?;
                    if p.len() != 2 {
                        return Err("invalid wire pair".into());
                    }
                    Ok((
                        db.templates[a].wire(p[0].string()?)?,
                        db.templates[b].wire(p[1].string()?)?,
                    ))
                })
                .collect::<Result<Vec<_>, String>>()?;
            for &i in &by_type[a] {
                let t = &db.tiles[i];
                let target = (
                    i16::try_from(i64::from(t.x) + dx).map_err(|_| "tile delta overflow")?,
                    i16::try_from(i64::from(t.y) + dy).map_err(|_| "tile delta overflow")?,
                );
                let Some(&j) = grid.get(&target) else { continue };
                let u = &db.tiles[j];
                if u.template != b {
                    continue;
                }
                for &(wa, wb) in &pairs {
                    let ra = root_node(&mut parent, t.nodes[wa as usize]);
                    let rb = root_node(&mut parent, u.nodes[wb as usize]);
                    if ra != rb {
                        parent[ra.max(rb) as usize] = ra.min(rb);
                    }
                }
            }
        }
        let mut ids = vec![u32::MAX; total as usize];
        for t in &mut db.tiles {
            for n in &mut t.nodes {
                let r = root_node(&mut parent, *n) as usize;
                if ids[r] == u32::MAX {
                    ids[r] = db.node_x.len() as u32;
                    db.node_x.push(t.x);
                    db.node_y.push(t.y);
                }
                *n = ids[r];
            }
        }
        Ok(db)
    }
    pub fn wire(&self, tile: usize, name: &str) -> Result<u32, String> {
        let t = &self.tiles[tile];
        Ok(t.nodes[self.templates[t.template].wire(name)? as usize])
    }
    /// Match relative site coordinates to the tile's absolute site names.
    pub fn sites(&self, tile: usize) -> Result<Vec<(String, usize)>, String> {
        let t = &self.tiles[tile];
        let template = &self.templates[t.template];
        let mut result = Vec::new();
        for (i, s) in template.sites.iter().enumerate() {
            let (rx, ry) = xy(&s.name)?;
            let sites: Vec<_> =
                t.sites.keys().filter(|n| n.starts_with(&format!("{}_X", s.prefix))).collect();
            let coords = sites.iter().map(|n| xy(n)).collect::<Result<Vec<_>, _>>()?;
            if coords.is_empty() {
                return Err(format!("missing {} sites in {}", s.prefix, t.name));
            }
            let minx = coords.iter().map(|p| p.0).min().unwrap();
            let miny = coords.iter().map(|p| p.1).min().unwrap();
            let name = format!("{}_X{}Y{}", s.prefix, minx + rx, miny + ry);
            if !t.sites.contains_key(&name) {
                return Err(format!("site template mismatch: {name} in {}", t.name));
            }
            result.push((name, i));
        }
        Ok(result)
    }
}
fn root_node(parent: &mut [u32], mut n: u32) -> u32 {
    while parent[n as usize] != n {
        parent[n as usize] = parent[parent[n as usize] as usize];
        n = parent[n as usize];
    }
    n
}
pub fn xy(name: &str) -> Result<(u32, u32), String> {
    let s = name.rsplit_once('X').map(|(_, s)| s).ok_or_else(|| format!("invalid site {name}"))?;
    let (x, y) = s.split_once('Y').ok_or("invalid site coordinate")?;
    Ok((x.parse().map_err(|_| "invalid site x")?, y.parse().map_err(|_| "invalid site y")?))
}
impl Template {
    pub fn wire(&self, name: &str) -> Result<u32, String> {
        self.wires
            .binary_search_by(|w| w.as_str().cmp(name))
            .map(|i| i as u32)
            .map_err(|_| format!("{}: unknown wire {name}", self.kind))
    }
    fn load(root: &Path, kind: &str) -> Result<Self, String> {
        let path = root.join(format!("tile_type_{kind}.json"));
        let j = read_json(&path)?;
        let number = |v: Option<&Value>| match v {
            Some(Value::String(s)) => s.parse::<f32>().ok().filter(|x| x.is_finite()),
            _ => None,
        };
        let wires = j.get("wires")?.object()?;
        let mut t = Self {
            kind: kind.into(),
            wires: wires.keys().cloned().collect(),
            wire_rc: wires
                .values()
                .map(|w| match w {
                    Value::Object(o) => {
                        (number(o.get("res")).unwrap_or(0.0), number(o.get("cap")).unwrap_or(0.0))
                    }
                    _ => (0.0, 0.0),
                })
                .collect(),
            sites: Vec::new(),
            pips: Vec::new(),
        };
        let mut known = BTreeSet::new();
        let mut fixed = BTreeSet::new();
        for prefix in ["segbits", "ppips"] {
            let path = root.join(format!("{prefix}_{}.db", kind.to_lowercase()));
            match synth_core::fs::read_to_string(&path) {
                Ok(s) => {
                    for line in s.lines() {
                        let mut words = line.split_whitespace();
                        let Some(feature) = words.next() else { continue };
                        if prefix == "ppips" {
                            // `always` pips need no configuration; `default`
                            // ones (VCC_WIRE into an interconnect mux) are what
                            // a mux with nothing selected drives.
                            if matches!(words.next(), Some("always" | "default")) {
                                fixed.insert(feature.to_string());
                            }
                        } else {
                            known.insert(feature.to_string());
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        for s in j.get("sites")?.array()? {
            let mut pins = BTreeMap::new();
            for (p, v) in s.get("site_pins")?.object()? {
                if matches!(v, Value::Null) {
                    continue;
                }
                let w =
                    if let Value::String(w) = v { w.as_str() } else { v.get("wire")?.string()? };
                pins.insert(p.clone(), t.wire(w)?);
            }
            t.sites.push(SiteTemplate {
                name: s.get("name")?.string()?.into(),
                prefix: s.get("prefix")?.string()?.into(),
                kind: s.get("type")?.string()?.into(),
                pins,
            });
        }
        for p in j.get("pips")?.object()?.values() {
            let (src, dst) = (p.get("src_wire")?.string()?, p.get("dst_wire")?.string()?);
            // A bidirectional pip is configured separately for each direction,
            // as `dst.src` of the wire it drives.
            let mut directions = vec![(src, dst, "src_to_dst")];
            if p.get("is_directional")?.integer()? == 0 {
                directions.push((dst, src, "dst_to_src"));
            }
            for (src, dst, timing) in directions {
                if !crate::routing::general_routing(kind, src, dst) {
                    continue;
                }
                let feature = format!("{kind}.{dst}.{src}");
                let extra = crate::routing::pseudo_config(kind, src, dst);
                let is_fixed = fixed.contains(&feature);
                if !known.contains(&feature) && !is_fixed && extra.is_none() {
                    continue;
                }
                let name: String = feature[kind.len() + 1..].into();
                let config =
                    extra.unwrap_or_else(|| if is_fixed { Vec::new() } else { vec![name.clone()] });
                let arc = p.get(timing)?;
                let field = |k: &str| match arc {
                    Value::Object(o) => number(o.get(k)),
                    _ => None,
                };
                t.pips.push(Pip {
                    src: t.wire(src)?,
                    dst: t.wire(dst)?,
                    feature: name,
                    delay: pip_delay(arc.get("delay")?)?
                        .or_else(|| untimed_pip_delay(kind, src, dst)),
                    res: field("res").unwrap_or(0.0),
                    cap: field("in_cap").unwrap_or(0.0),
                    buffered: !matches!(
                        p.get("is_pass_transistor"),
                        Ok(Value::Number(v) | Value::String(v)) if v == "1"
                    ),
                    fixed: is_fixed,
                    config,
                });
            }
        }
        Ok(t)
    }
}

/// Routing delays of Project X-Ray's timing model scale by this factor to
/// Vivado 2025.2's slow-corner net delays (fitted with `untimed_pip_delay`
/// over the routed connections of Euler designs, `tools/vivado_artix7.py
/// --delays`).
pub const VENDOR_SCALE: f32 = 1.144;

/// Intrinsic delays (ps, before `VENDOR_SCALE`) of pips Project X-Ray leaves
/// untimed, fitted to Vivado 2025.2's slow-corner net delays: the
/// interconnect-to-slice input pips, which differ by LUT input (A6 fastest,
/// as the vendor router uses it), slice outputs through the output muxes,
/// and the IOB input path. Other untimed pips (direct outputs, bounce and
/// alternate interconnect wires) fit to about zero.
fn untimed_pip_delay(kind: &str, src: &str, dst: &str) -> Option<f32> {
    let lane_pin = |w: &str| w.rsplit('_').next().unwrap_or("").to_string();
    let ps = if kind.starts_with("CLBL") {
        let (from, to) = (lane_pin(src), lane_pin(dst));
        match to.as_bytes() {
            [b'A'..=b'D', d @ b'1'..=b'6'] => {
                [515.0, 494.0, 340.0, 275.0, 114.0, 0.0][(d - b'1') as usize]
            }
            _ if to == "CE" => 172.0,
            _ if to == "SR" => 38.0,
            [b'A'..=b'D', b'X'] => 73.0,
            _ if from.len() == 4 && from.ends_with("MUX") => 185.0,
            _ => return None,
        }
    } else if (kind.starts_with("LIOI3") || kind.starts_with("RIOI3"))
        && (src.contains("_IBUF") || dst.contains("ILOGIC") && dst.ends_with("_D"))
    {
        193.0
    } else {
        return None;
    };
    Some(ps / VENDOR_SCALE)
}

fn pip_delay(value: &Value) -> Result<Option<f32>, String> {
    match value {
        Value::Array(values) => {
            if values.len() != 4 {
                return Err("pip timing requires four corners".into());
            }
            let mut worst = 0.0f32;
            for v in values {
                let d: f32 = v.string()?.parse().map_err(|_| "invalid pip delay")?;
                if !d.is_finite() || d < 0.0 || !(d * 1000.0).is_finite() {
                    return Err("pip delay must be finite and nonnegative".into());
                }
                worst = worst.max(d * 1000.0);
            }
            Ok(Some(worst))
        }
        Value::Null => Ok(None),
        _ => Err("invalid pip timing".into()),
    }
}
