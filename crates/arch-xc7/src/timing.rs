//! X-Ray SDF reader and timing-coverage report.
//!
//! Missing characterization is explicit. Routing heuristics must never be
//! substituted for measured delays when deciding whether a clock meets timing.
use crate::{device::Device, routing::RoutingGraph};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct CellTiming {
    pub propagation: BTreeMap<(String, String), (f64, f64)>,
    pub setup: BTreeMap<(String, String, String), f64>,
    pub hold: BTreeMap<(String, String, String), f64>,
    pub recovery: BTreeMap<(String, String, String), f64>,
    pub removal: BTreeMap<(String, String, String), f64>,
    pub interconnect: BTreeMap<(String, String), (f64, f64)>,
    pub device: BTreeMap<String, (f64, f64)>,
}
impl CellTiming {
    pub fn arc_count(&self) -> usize {
        self.propagation.len()
            + self.setup.len()
            + self.hold.len()
            + self.recovery.len()
            + self.removal.len()
            + self.interconnect.len()
            + self.device.len()
    }
}
#[derive(Clone, Debug, Default)]
pub struct Sdf {
    pub cells: BTreeMap<(String, String), CellTiming>,
}
impl Sdf {
    /// X-Ray's generated SDF has one timing arc per line. Reject unsupported
    /// timing records instead of silently omitting them from a timing model.
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut sdf = Self::default();
        let mut kind = String::new();
        let mut instance = String::new();
        let mut scale = None;
        for (i, line) in source.lines().enumerate() {
            let line = line.trim();
            let words: Vec<_> =
                line.split(['(', ')', ' ', '\t']).filter(|s| !s.is_empty()).collect();
            if words.is_empty() {
                continue;
            }
            let error = |msg: &str| format!("SDF line {}: {msg}", i + 1);
            match words[0] {
                "TIMESCALE" => {
                    scale = Some(match words.get(1).copied() {
                        Some("1ns") => 1.0,
                        Some("1ps") => 0.001,
                        _ => return Err(error("unsupported timing scale")),
                    });
                }
                "CELLTYPE" => {
                    kind = words
                        .get(1)
                        .ok_or_else(|| error("missing cell type"))?
                        .trim_matches('"')
                        .into()
                }
                "INSTANCE" => {
                    instance = (*words.get(1).ok_or_else(|| error("missing instance"))?).into()
                }
                "IOPATH" | "INTERCONNECT" => {
                    if scale.is_none() || kind.is_empty() || instance.is_empty() {
                        return Err(error("arc outside a characterized cell"));
                    }
                    if words.len() != 5 {
                        return Err(error("unsupported propagation arc form"));
                    }
                    let (a, b) = scaled_range(words[3], scale)?;
                    let (c, d) = scaled_range(words[4], scale)?;
                    let cell = sdf.cells.entry((instance.clone(), kind.clone())).or_default();
                    let table = if words[0] == "IOPATH" {
                        &mut cell.propagation
                    } else {
                        &mut cell.interconnect
                    };
                    table
                        .entry((words[1].into(), words[2].into()))
                        .and_modify(|v| {
                            v.0 = v.0.min(a.min(c));
                            v.1 = v.1.max(b.max(d));
                        })
                        .or_insert((a.min(c), b.max(d)));
                }
                "DEVICE" => {
                    if words.len() != 4 || kind.is_empty() || instance.is_empty() {
                        return Err(error("unsupported device delay"));
                    }
                    let (a, b) = scaled_range(words[2], scale)?;
                    let (c, d) = scaled_range(words[3], scale)?;
                    sdf.cells
                        .entry((instance.clone(), kind.clone()))
                        .or_default()
                        .device
                        .insert(words[1].into(), (a.min(c), b.max(d)));
                }
                "SETUP" | "HOLD" | "RECOVERY" | "REMOVAL" => {
                    if words.len() != 5 || !matches!(words[2], "posedge" | "negedge") {
                        return Err(error("unsupported timing check form"));
                    }
                    let (_, max) = scaled_range(words[4], scale)?;
                    let cell = sdf.cells.entry((instance.clone(), kind.clone())).or_default();
                    let table = match words[0] {
                        "SETUP" => &mut cell.setup,
                        "HOLD" => &mut cell.hold,
                        "RECOVERY" => &mut cell.recovery,
                        _ => &mut cell.removal,
                    };
                    table
                        .entry((words[1].into(), words[3].into(), words[2].into()))
                        .and_modify(|d| *d = d.max(max))
                        .or_insert(max);
                }
                "DELAYFILE" | "SDFVERSION" | "CELL" | "DELAY" | "ABSOLUTE" | "TIMINGCHECK" => {}
                _ => return Err(error(&format!("unsupported timing record {}", words[0]))),
            }
        }
        if scale.is_none() {
            return Err("SDF has no time scale".into());
        }
        Ok(sdf)
    }
}
fn scaled_range(s: &str, scale: Option<f64>) -> Result<(f64, f64), String> {
    let scale = scale.ok_or("timing arc precedes TIMESCALE")?;
    let (a, b) = range(s)?;
    Ok((a * scale, b * scale))
}
fn range(s: &str) -> Result<(f64, f64), String> {
    let values = s
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<f64>().map_err(|_| format!("invalid SDF delay {s}")))
        .collect::<Result<Vec<_>, _>>()?;
    if values.is_empty() || values.iter().any(|v| !v.is_finite()) {
        return Err("empty or nonfinite SDF delay".into());
    }
    Ok((
        values.iter().copied().reduce(f64::min).unwrap(),
        values.iter().copied().reduce(f64::max).unwrap(),
    ))
}
#[derive(Clone, Debug, Default)]
pub struct Coverage {
    pub characterized_cells: usize,
    pub characterized_arcs: usize,
    pub missing_pip_types: BTreeSet<String>,
    pub missing_tile_models: BTreeSet<String>,
}
impl Coverage {
    pub fn report(&self) -> String {
        format!(
            "timing characterization: {} SDF cells, {} arcs; {} routed pip types lack delay data; {} routed tile types lack a cell model",
            self.characterized_cells,
            self.characterized_arcs,
            self.missing_pip_types.len(),
            self.missing_tile_models.len()
        )
    }
}
/// Audit the actual routed graph, not just the set of primitive names. A
/// structurally connected clock tree can still have uncharacterized arcs.
pub fn coverage(
    root: &Path,
    db: &Device,
    graph: &RoutingGraph,
    routes: &[synth_route::NetRoute],
) -> Result<Coverage, String> {
    let mut result = Coverage::default();
    let mut used = BTreeSet::new();
    for r in routes {
        for &e in &r.edges {
            let (ti, pi) = graph.pip[e as usize];
            let tile = &db.tiles[ti as usize];
            let pip = &db.templates[tile.template].pips[pi as usize];
            used.insert(tile.kind.clone());
            if pip.delay.is_none() && !pip.fixed {
                result.missing_pip_types.insert(format!("{}.{}", tile.kind, pip.feature));
            }
        }
    }
    for kind in used {
        let path = root.join("timings").join(format!("{kind}.sdf"));
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let model = Sdf::parse(&s).map_err(|e| format!("{}: {e}", path.display()))?;
                result.characterized_cells += model.cells.len();
                result.characterized_arcs +=
                    model.cells.values().map(|c| c.arc_count()).sum::<usize>();
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !kind.starts_with("INT_") && !kind.starts_with("HCLK_") {
                    result.missing_tile_models.insert(kind);
                }
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_negative_setup_and_corners() {
        let s=Sdf::parse("(DELAYFILE\n(TIMESCALE 1ns)\n(CELLTYPE \"FF\")\n(INSTANCE SLICE)\n(IOPATH CLK Q (0.1::0.2)(0.3::0.4))\n(SETUP DIN (posedge CLK) (-0.057::-0.046))\n(HOLD DIN (posedge CLK) (0.181::0.225))\n)").unwrap();
        let c = &s.cells[&("SLICE".into(), "FF".into())];
        assert_eq!(c.setup[&("DIN".into(), "CLK".into(), "posedge".into())], -0.046);
        assert_eq!(c.hold[&("DIN".into(), "CLK".into(), "posedge".into())], 0.225);
        assert_eq!(c.propagation[&("CLK".into(), "Q".into())], (0.1, 0.4));
        assert!(Sdf::parse("(TIMESCALE 1ns)\n(IOPATH A Y (NaN)(0))").is_err());
    }
}
