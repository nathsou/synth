//! A device with everything the flow needs: its database, routing graph,
//! timing and logic tile grid.

use std::path::Path;

use crate::cells::{DP16KD, MULT18X18D};
use crate::clocks::Geometry;
use crate::db::{Database, Device, Pin};
use crate::graph::Graph;
use crate::place::Grid;
use crate::speed::Speed;

#[derive(Debug)]
pub struct Arch {
    pub db: Database,
    pub dev: Device,
    pub graph: Graph,
    pub speed: Speed,
    pub grid: Grid,
    pub clocks: Geometry,
    /// The package (upper case, as `iodb.json` names it).
    pub package: String,
}

impl Arch {
    /// Loads `device` (`LFE5U-25F`, ...) in `package` (`CABGA381`, ...) at
    /// speed grade `speed` from the database at `root`.
    pub fn load(root: &Path, device: &str, package: &str, speed: &str) -> Result<Arch, String> {
        let db = Database::open(root)?;
        let dev = db.device(device)?;
        let package = package.to_ascii_uppercase();
        if !dev.packages.contains_key(&package) {
            let mut known: Vec<&String> = dev.packages.keys().collect();
            known.sort();
            return Err(format!(
                "{device} has no package `{package}` (known: {})",
                known.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        let speed = Speed::load(root, speed)?;
        let graph = Graph::build(&dev, &db, &speed)?;
        let logic: Vec<(usize, usize)> =
            dev.tiles.iter().filter(|t| t.ttype == "PLC2").map(|t| (t.col, t.row)).collect();
        let blocks: Vec<(usize, usize, usize, &str)> = dev
            .tiles
            .iter()
            .filter_map(|t| {
                let ebr = crate::bitgen::ebr_z(&t.ttype).map(|z| (t.col, t.row, z, DP16KD));
                ebr.or_else(|| {
                    crate::bitgen::dsp_z(&t.ttype).map(|z| (t.col, t.row, z, MULT18X18D))
                })
            })
            .collect();
        let grid = Grid::new(logic, blocks);
        let clocks = Geometry::from_json(&dev.globals);
        Ok(Arch { db, dev, graph, speed, grid, clocks, package })
    }

    /// The pad of a package pin.
    pub fn pin(&self, name: &str) -> Option<&Pin> {
        self.dev.packages.get(&self.package)?.get(name)
    }

    /// The I/O bank of a pad.
    pub fn bank(&self, pin: &Pin) -> Option<u8> {
        self.dev.banks.get(&(pin.row, pin.col, pin.pio)).copied()
    }

    /// The node of wire `name` at a tile position.
    pub fn node(&self, x: usize, y: usize, name: &str) -> Result<u32, String> {
        self.graph.node(x, y, name).ok_or_else(|| format!("no wire R{y}C{x}_{name}"))
    }
}
