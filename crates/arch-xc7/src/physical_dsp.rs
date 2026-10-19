//! Physical support for DSP48E1 multipliers with optional input, product and
//! output registers, adding zero or C. Other DSP modes fail explicitly until
//! their control, register and cascade semantics are modeled.
use crate::{
    device::{Device, xy},
    pack::{param, value},
    place::Site,
};
use std::collections::BTreeSet;
use synth_netlist::{CellId, Dir, NetId, Netlist};

/// The DSP48E1 modes the physical flow implements: a multiplier with
/// optional A, B, C, M and P registers, adding nothing or C.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub areg: bool,
    pub breg: bool,
    pub creg: bool,
    pub mreg: bool,
    pub preg: bool,
    /// The post-adder adds C (OPMODE Z = C), rather than zero.
    pub add_c: bool,
}

impl Config {
    /// Whether any register is used, and so the clock.
    pub fn clocked(&self) -> bool {
        self.areg || self.breg || self.creg || self.mreg || self.preg
    }
}

/// Each register parameter must be 0 or 1 (default 1, like the primitive's).
fn register(nl: &Netlist, c: CellId, name: &str) -> Result<bool, String> {
    match param(nl, c, name, 1) {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(format!("DSP48E1 {name}: physical flow supports at most one register stage")),
    }
}

pub fn validate(nl: &Netlist, c: CellId) -> Result<Config, String> {
    let config = Config {
        areg: register(nl, c, "AREG")?,
        breg: register(nl, c, "BREG")?,
        creg: register(nl, c, "CREG")?,
        mreg: register(nl, c, "MREG")?,
        preg: register(nl, c, "PREG")?,
        add_c: false,
    };
    // Cascade registers match the input registers they follow.
    for (cascade, reg, used) in
        [("ACASCREG", "AREG", config.areg), ("BCASCREG", "BREG", config.breg)]
    {
        if register(nl, c, cascade)? != used {
            return Err(format!("DSP48E1 {cascade} must equal {reg}"));
        }
    }
    for p in
        ["ADREG", "ALUMODEREG", "CARRYINREG", "CARRYINSELREG", "DREG", "INMODEREG", "OPMODEREG"]
    {
        let unused = matches!(p, "ADREG" | "DREG");
        if !unused && register(nl, c, p)? {
            return Err(format!("DSP48E1 {p}: physical flow supports unregistered controls"));
        }
    }
    for (p, want) in [
        ("A_INPUT", "DIRECT"),
        ("B_INPUT", "DIRECT"),
        ("USE_DPORT", "FALSE"),
        ("USE_MULT", "MULTIPLY"),
        ("USE_SIMD", "ONE48"),
    ] {
        if nl.param(c, p).is_some_and(|v| v.as_str() != Some(want)) {
            return Err(format!("unsupported DSP48E1 {p}"));
        }
    }
    let constant = |p: &str, width: usize| -> Result<u64, String> {
        (0..width).try_fold(0, |acc, i| {
            let name = if width == 1 { p.into() } else { format!("{p}[{i}]") };
            match nl.pin_net(c, &name).and_then(|n| value(nl, n)) {
                Some(v) => Ok(acc | (v as u64) << i),
                None => Err(format!("DSP48E1 requires constant control {name}")),
            }
        })
    };
    // X = Y = M (the product); Z = 0 or C.
    let add_c = match constant("OPMODE", 7)? {
        0b000_0101 => false,
        0b011_0101 => true,
        _ => return Err("DSP48E1 OPMODE: physical flow supports P = A*B or A*B + C".into()),
    };
    for (p, width) in [("ALUMODE", 4), ("INMODE", 5), ("CARRYINSEL", 3), ("CARRYIN", 1)] {
        if constant(p, width)? != 0 {
            return Err(format!("DSP48E1 requires {p} = 0"));
        }
    }
    for (p, v) in nl.cells[c].params.iter() {
        if nl.str(*p).starts_with("IS_") && v.as_u64() != Some(0) {
            return Err(format!("unsupported DSP48E1 inversion {}", nl.str(*p)));
        }
    }
    for pin in &nl.cells[c].pins {
        if pin.dir == Dir::Output
            && !nl.str(pin.name).starts_with("P[")
            && pin.net.is_some_and(|n| !nl.nets[n].users.is_empty())
        {
            return Err("DSP48E1 physical multiplier supports only P outputs".into());
        }
    }
    Ok(Config { add_c, ..config })
}
/// Whether input `name` affects the configured function.
fn active_pin(config: &Config, name: &str) -> bool {
    let bus =
        |prefix: &str| name.starts_with(prefix) && name.as_bytes().get(prefix.len()) == Some(&b'[');
    // The clock enables and resets of the registers used.
    let control = |reg: bool, ce: &str, rst: &str| reg && (name == ce || name == rst);
    (bus("A") && !matches!(name, "A[25]" | "A[26]" | "A[27]" | "A[28]" | "A[29]"))
        || bus("B")
        || (config.add_c && bus("C"))
        || ["OPMODE", "ALUMODE", "INMODE", "CARRYINSEL"].iter().any(|b| bus(b))
        || name == "CARRYIN"
        || (config.clocked() && name == "CLK")
        || control(config.areg, "CEA2", "RSTA")
        || control(config.breg, "CEB2", "RSTB")
        || control(config.add_c && config.creg, "CEC", "RSTC")
        || control(config.mreg, "CEM", "RSTM")
        || control(config.preg, "CEP", "RSTP")
}
fn tie(db: &Device, site: &Site, pin: &str, v: bool) -> Option<String> {
    let tile = &db.tiles[site.tile];
    let template = &db.templates[tile.template];
    let y = xy(&template.sites[site.index].name).ok()?.1;
    let side = tile.kind.strip_prefix("DSP_")?;
    let f = format!("DSP_{y}_{pin}.DSP_{}_{side}", if v { "VCC" } else { "GND" });
    template.pips.iter().any(|p| p.feature == f).then(|| format!("{}.{f}", tile.name))
}
pub fn inputs(
    db: &Device,
    nl: &Netlist,
    c: CellId,
    site: &Site,
) -> Result<Vec<(String, NetId)>, String> {
    let config = validate(nl, c)?;
    Ok(nl.cells[c]
        .pins
        .iter()
        .filter_map(|p| {
            let name = nl.str(p.name);
            let n = p.net?;
            if p.dir != Dir::Input || !active_pin(&config, name) {
                return None;
            }
            let pin = name.replace(['[', ']'], "");
            if value(nl, n).is_some_and(|v| tie(db, site, &pin, v).is_some()) {
                return None;
            }
            Some((pin, n))
        })
        .collect())
}
pub fn features(
    db: &Device,
    nl: &Netlist,
    c: CellId,
    site: &Site,
) -> Result<BTreeSet<String>, String> {
    let config = validate(nl, c)?;
    let tile = &db.tiles[site.tile];
    let y = xy(&db.templates[tile.template].sites[site.index].name)?.1;
    let prefix = format!("{}.DSP48.DSP_{y}", tile.name);
    let mut f = BTreeSet::new();
    // A register stage is the default; zero stages have a feature.
    for (used, key) in [(config.areg, "AREG_0"), (config.breg, "BREG_0")] {
        if !used {
            f.insert(format!("{prefix}.{key}"));
        }
    }
    for key in
        ["ZAREG_2_ACASCREG_1", "ZBREG_2_BCASCREG_1", "ZIS_CLK_INVERTED", "ZIS_CARRYIN_INVERTED"]
    {
        f.insert(format!("{prefix}.{key}"));
    }
    for (used, key) in [
        (false, "ADREG"),
        (false, "ALUMODEREG"),
        (false, "CARRYINREG"),
        (false, "CARRYINSELREG"),
        (config.creg, "CREG"),
        (false, "DREG"),
        (false, "INMODEREG"),
        (config.mreg, "MREG"),
        (false, "OPMODEREG"),
        (config.preg, "PREG"),
    ] {
        if !used {
            f.insert(format!("{prefix}.Z{key}[0]"));
        }
    }
    for (bus, width) in [("ALUMODE", 4), ("INMODE", 5), ("OPMODE", 7)] {
        for i in 0..width {
            f.insert(format!("{prefix}.ZIS_{bus}_INVERTED[{i}]"));
        }
    }
    // Pattern detection outputs are unused; preserve the documented default.
    for i in 0..46 {
        f.insert(format!("{prefix}.MASK[{i}]"));
    }
    for pin in &nl.cells[c].pins {
        if pin.dir != Dir::Input || !active_pin(&config, nl.str(pin.name)) {
            continue;
        }
        if let Some(v) = pin.net.and_then(|n| value(nl, n))
            && let Some(feature) = tie(db, site, &nl.str(pin.name).replace(['[', ']'], ""), v)
        {
            f.insert(feature);
        }
    }
    Ok(f)
}
