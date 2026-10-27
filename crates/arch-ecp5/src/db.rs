//! The Project Trellis database of the ECP5 family: devices, their tile
//! grids and package pins, and each tile type's configuration bits.
//!
//! The database is read at run time from its install location (`--db`,
//! `SYNTH_TRELLIS_DB`, or a system path such as Debian's
//! `/usr/share/trellis/database`), the directory holding `devices.json` and
//! `ECP5/`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use synth_core::{FxHashMap, FxHashSet, Json};

use crate::bits::{Cram, TileBits, Window};
use crate::config::{ChipConfig, TileConfig};

/// Where packaged databases are installed.
const SYSTEM_PATHS: [&str; 2] =
    ["/usr/share/trellis/database", "/usr/local/share/trellis/database"];

/// The database root: `explicit`, else `SYNTH_TRELLIS_DB`, else the first
/// system path holding a database.
pub fn locate(explicit: Option<&Path>) -> Result<PathBuf, String> {
    let has_db = |p: &Path| p.join("devices.json").is_file() && p.join("ECP5").is_dir();
    if let Some(p) = explicit {
        return if has_db(p) {
            Ok(p.to_path_buf())
        } else {
            Err(format!(
                "{}: not a Project Trellis database (no devices.json and ECP5/)",
                p.display()
            ))
        };
    }
    if let Some(p) = std::env::var_os("SYNTH_TRELLIS_DB") {
        return locate(Some(Path::new(&p)));
    }
    SYSTEM_PATHS.iter().map(Path::new).find(|p| has_db(p)).map(Path::to_path_buf).ok_or_else(|| {
        "no Project Trellis database found: pass --db <path to prjtrellis-db>, set \
         SYNTH_TRELLIS_DB, or install it (Debian and Ubuntu: fpga-trellis-database)"
            .to_string()
    })
}

fn read_json(path: &Path) -> Result<Json, String> {
    let text =
        synth_core::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Json::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn int(j: &Json, key: &str) -> Result<i64, String> {
    j.get(key).and_then(Json::as_i64).ok_or_else(|| format!("missing number `{key}`"))
}

/// A device's configuration geometry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipInfo {
    pub name: String,
    pub idcode: u32,
    pub frames: usize,
    pub bits_per_frame: usize,
    pub pad_bits_before_frame: usize,
    pub pad_bits_after_frame: usize,
    /// The largest tile row and column.
    pub max_row: usize,
    pub max_col: usize,
    pub packages: Vec<String>,
}

/// A tile: its full name (`R2C3:PLC2`), type, position and window into
/// the configuration memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileInfo {
    pub name: String,
    pub ttype: String,
    pub row: usize,
    pub col: usize,
    pub window: Window,
}

/// A package pin: the I/O tile position and pad letter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub row: usize,
    pub col: usize,
    /// `A` to `D`.
    pub pio: char,
}

/// The parsed database: devices and the bit databases of tile types,
/// loaded once each.
#[derive(Debug)]
pub struct Database {
    pub root: PathBuf,
    devices: Json,
    bits: Mutex<FxHashMap<String, Arc<TileBits>>>,
}

impl Database {
    pub fn open(root: &Path) -> Result<Database, String> {
        let devices = read_json(&root.join("devices.json"))?;
        Ok(Database { root: root.to_path_buf(), devices, bits: Mutex::default() })
    }

    /// The ECP5 devices the database has.
    pub fn device_names(&self) -> Vec<String> {
        match self.devices.at(&["families", "ECP5", "devices"]) {
            Json::Obj(m) => m.iter().map(|(k, _)| k.clone()).collect(),
            _ => Vec::new(),
        }
    }

    pub fn chip_info(&self, device: &str) -> Result<ChipInfo, String> {
        let d = self.devices.at(&["families", "ECP5", "devices", device]);
        if d.is_null() {
            return Err(format!(
                "unknown ECP5 device `{device}` (known: {})",
                self.device_names().join(", ")
            ));
        }
        let idcode = d.get("idcode").and_then(Json::as_str).ok_or("missing idcode")?;
        let idcode = u32::from_str_radix(idcode.trim_start_matches("0x"), 16)
            .map_err(|_| format!("bad idcode `{idcode}`"))?;
        Ok(ChipInfo {
            name: device.to_string(),
            idcode,
            frames: int(d, "frames")? as usize,
            bits_per_frame: int(d, "bits_per_frame")? as usize,
            pad_bits_before_frame: int(d, "pad_bits_before_frame")? as usize,
            pad_bits_after_frame: int(d, "pad_bits_after_frame")? as usize,
            max_row: int(d, "max_row")? as usize,
            max_col: int(d, "max_col")? as usize,
            packages: d.get("packages").map_or(Vec::new(), |p| {
                p.as_array().iter().filter_map(|s| s.as_str().map(str::to_string)).collect()
            }),
        })
    }

    /// The configuration bits of a tile type.
    pub fn tile_bits(&self, ttype: &str) -> Result<Arc<TileBits>, String> {
        if let Some(b) = self.bits.lock().expect("tile databases").get(ttype) {
            return Ok(b.clone());
        }
        if !ttype.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            return Err(format!("bad tile type `{ttype}`"));
        }
        let path = self.root.join("ECP5").join("tiledata").join(ttype).join("bits.db");
        let text = synth_core::fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let bits =
            Arc::new(TileBits::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?);
        self.bits.lock().expect("tile databases").insert(ttype.to_string(), bits.clone());
        Ok(bits)
    }

    /// A device's tiles, package pins and global clock geometry.
    pub fn device(&self, name: &str) -> Result<Device, String> {
        let info = self.chip_info(name)?;
        let dir = self.root.join("ECP5").join(name);
        let grid = read_json(&dir.join("tilegrid.json"))?;
        let Json::Obj(entries) = grid else { return Err("tilegrid.json: not an object".into()) };
        let mut tiles = Vec::with_capacity(entries.len());
        for (tname, t) in &entries {
            let (row, col) =
                row_col(tname).ok_or_else(|| format!("tile `{tname}` has no position"))?;
            let window = Window {
                frame: int(t, "start_frame")? as usize,
                bit: int(t, "start_bit")? as usize,
                frames: int(t, "cols")? as usize,
                bits: int(t, "rows")? as usize,
            };
            if window.frame + window.frames > info.frames
                || window.bit + window.bits > info.bits_per_frame
            {
                return Err(format!("tile `{tname}` lies outside the configuration memory"));
            }
            let ttype =
                t.get("type").and_then(Json::as_str).ok_or("tile without type")?.to_string();
            tiles.push(TileInfo { name: tname.clone(), ttype, row, col, window });
        }
        tiles.sort_by(|a, b| a.name.cmp(&b.name));
        let io = read_json(&dir.join("iodb.json"))?;
        let mut packages = FxHashMap::default();
        if let Json::Obj(pkgs) = io.at(&["packages"]) {
            for (pkg, pins) in pkgs {
                let mut map = FxHashMap::default();
                if let Json::Obj(pins) = pins {
                    for (pin, p) in pins {
                        let pio =
                            p.get("pio").and_then(Json::as_str).and_then(|s| s.chars().next());
                        map.insert(
                            pin.clone(),
                            Pin {
                                row: int(p, "row")? as usize,
                                col: int(p, "col")? as usize,
                                pio: pio.ok_or("pin without pio")?,
                            },
                        );
                    }
                }
                packages.insert(pkg.clone(), map);
            }
        }
        let mut banks = FxHashMap::default();
        for p in io.at(&["pio_metadata"]).as_array() {
            let pio =
                p.get("pio").and_then(Json::as_str).and_then(|s| s.chars().next()).unwrap_or('A');
            banks.insert(
                (int(p, "row")? as usize, int(p, "col")? as usize, pio),
                int(p, "bank")? as u8,
            );
        }
        let globals = read_json(&dir.join("globals.json"))?;
        Ok(Device::new(info, tiles, packages, banks, globals))
    }
}

/// The row and column of a tile, from the first `R<row>C<col>` in its name.
pub fn row_col(name: &str) -> Option<(usize, usize)> {
    let b = name.as_bytes();
    let digits = |mut i: usize| {
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        (i > start).then(|| (name[start..i].parse::<usize>().ok(), i))
    };
    for i in 0..b.len() {
        if b[i] == b'R'
            && let Some((Some(row), j)) = digits(i + 1)
            && b.get(j) == Some(&b'C')
            && let Some((Some(col), _)) = digits(j + 1)
        {
            return Some((row, col));
        }
    }
    None
}

/// A device: its tiles, package pins and I/O banks.
#[derive(Debug)]
pub struct Device {
    pub info: ChipInfo,
    /// Sorted by name.
    pub tiles: Vec<TileInfo>,
    by_name: FxHashMap<String, usize>,
    /// Tiles at each `(row, col)`, by `row * (max_col + 1) + col`.
    at: Vec<Vec<u32>>,
    /// Package name (upper case) to pin name to pad.
    pub packages: FxHashMap<String, FxHashMap<String, Pin>>,
    /// The I/O bank of each pad.
    pub banks: FxHashMap<(usize, usize, char), u8>,
    /// `globals.json`: clock quadrants, tap drivers and spines.
    pub globals: Json,
}

impl Device {
    fn new(
        info: ChipInfo,
        tiles: Vec<TileInfo>,
        packages: FxHashMap<String, FxHashMap<String, Pin>>,
        banks: FxHashMap<(usize, usize, char), u8>,
        globals: Json,
    ) -> Device {
        let by_name = tiles.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();
        let mut at = vec![Vec::new(); (info.max_row + 1) * (info.max_col + 1)];
        for (i, t) in tiles.iter().enumerate() {
            if t.row <= info.max_row && t.col <= info.max_col {
                at[t.row * (info.max_col + 1) + t.col].push(i as u32);
            }
        }
        Device { info, tiles, by_name, at, packages, banks, globals }
    }

    pub fn tile(&self, name: &str) -> Option<&TileInfo> {
        self.by_name.get(name).map(|&i| &self.tiles[i])
    }

    /// The tiles at a position.
    pub fn tiles_at(&self, row: usize, col: usize) -> impl Iterator<Item = &TileInfo> {
        let ids: &[u32] = if row <= self.info.max_row && col <= self.info.max_col {
            &self.at[row * (self.info.max_col + 1) + col]
        } else {
            &[]
        };
        ids.iter().map(|&i| &self.tiles[i as usize])
    }

    /// The tile of one of `types` at a position.
    pub fn tile_of_type(&self, row: usize, col: usize, types: &[&str]) -> Option<&TileInfo> {
        self.tiles_at(row, col).find(|t| types.contains(&t.ttype.as_str()))
    }

    /// The configuration memory a configuration describes. Tiles it does
    /// not name take their defaults (not always all-zero: I/O tiles have
    /// set bits by default).
    pub fn to_cram(&self, db: &Database, cc: &ChipConfig) -> Result<Cram, String> {
        if cc.device != self.info.name {
            return Err(format!("configuration is for {}, not {}", cc.device, self.info.name));
        }
        for name in cc.tiles.keys() {
            if self.tile(name).is_none() {
                return Err(format!("tile {name} does not exist in {}", self.info.name));
            }
        }
        let mut cram = Cram::new(self.info.frames, self.info.bits_per_frame);
        let empty = TileConfig::default();
        for t in &self.tiles {
            let cfg = cc.tiles.get(&t.name).unwrap_or(&empty);
            db.tile_bits(&t.ttype)?
                .apply(cfg, &mut cram, &t.window, None)
                .map_err(|e| format!("tile {}: {e}", t.name))?;
        }
        for g in &cc.groups {
            let mut matched = FxHashSet::default();
            for name in &g.tiles {
                let t = self.tile(name).ok_or_else(|| format!("tile {name} does not exist"))?;
                db.tile_bits(&t.ttype)?
                    .apply(&g.config, &mut cram, &t.window, Some(&mut matched))
                    .map_err(|e| format!("tile {name}: {e}"))?;
            }
            let names =
                g.config.words.iter().map(|w| &w.0).chain(g.config.enums.iter().map(|e| &e.0));
            for name in names {
                if !matched.contains(name) {
                    return Err(format!("setting {name} matched in no tile of its group"));
                }
            }
        }
        Ok(cram)
    }

    /// The configuration a configuration memory holds, read tile by tile.
    pub fn from_cram(&self, db: &Database, cram: &Cram) -> Result<ChipConfig, String> {
        let mut cc = ChipConfig { device: self.info.name.clone(), ..ChipConfig::default() };
        for t in &self.tiles {
            let cfg = db.tile_bits(&t.ttype)?.read(cram, &t.window);
            cc.tiles.insert(t.name.clone(), cfg);
        }
        Ok(cc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_positions_come_from_names() {
        assert_eq!(row_col("CIB_R10C71:CIB_LR"), Some((10, 71)));
        assert_eq!(row_col("R2C3:PLC2"), Some((2, 3)));
        assert_eq!(row_col("MIB_R0C3:PIOT0"), Some((0, 3)));
        assert_eq!(row_col("RMID_0"), None);
    }
}
