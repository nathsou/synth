//! Physical support for combinational DSP48E1 multipliers. Other DSP modes
//! fail explicitly until their control/register/cascade semantics are modeled.
use crate::{
    device::{Device, xy},
    pack::{param, value},
    place::Site,
};
use std::collections::BTreeSet;
use synth_netlist::{CellId, Dir, NetId, Netlist};

pub fn validate(nl: &Netlist, c: CellId) -> Result<(), String> {
    for &p in crate::dsp::REGISTERS {
        if param(nl, c, p, 1) != 0 {
            return Err(format!(
                "DSP48E1 {p}: physical flow currently supports unregistered multipliers"
            ));
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
    for (p, width, want) in [
        ("OPMODE", 7, 5),
        ("ALUMODE", 4, 0),
        ("INMODE", 5, 0),
        ("CARRYINSEL", 3, 0),
        ("CARRYIN", 1, 0),
    ] {
        for i in 0..width {
            let name = if width == 1 { p.into() } else { format!("{p}[{i}]") };
            if nl.pin_net(c, &name).and_then(|n| value(nl, n)) != Some(want >> i & 1 != 0) {
                return Err(format!("DSP48E1 requires fixed multiplier control {name}"));
            }
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
    Ok(())
}
fn active_pin(name: &str) -> bool {
    if name == "CARRYIN" {
        return true;
    }
    ["A[", "B[", "OPMODE[", "ALUMODE[", "INMODE[", "CARRYINSEL["]
        .iter()
        .any(|s| name.starts_with(s))
        && !matches!(name, "A[25]" | "A[26]" | "A[27]" | "A[28]" | "A[29]")
}
fn tie(db: &Device, site: &Site, pin: &str, v: bool) -> Option<String> {
    let tile = &db.tiles[site.tile];
    let template = &db.templates[tile.template];
    let y = xy(&template.sites[site.index].name).ok()?.1;
    let side = tile.kind.strip_prefix("DSP_")?;
    let f = format!("DSP_{y}_{pin}.DSP_{}_{side}", if v { "VCC" } else { "GND" });
    template.pips.iter().any(|p| p.feature == f).then(|| format!("{}.{f}", tile.name))
}
pub fn inputs(db: &Device, nl: &Netlist, c: CellId, site: &Site) -> Vec<(String, NetId)> {
    nl.cells[c]
        .pins
        .iter()
        .filter_map(|p| {
            let name = nl.str(p.name);
            let n = p.net?;
            if p.dir != Dir::Input || !active_pin(name) {
                return None;
            }
            let pin = name.replace(['[', ']'], "");
            if value(nl, n).is_some_and(|v| tie(db, site, &pin, v).is_some()) {
                return None;
            }
            Some((pin, n))
        })
        .collect()
}
pub fn features(
    db: &Device,
    nl: &Netlist,
    c: CellId,
    site: &Site,
) -> Result<BTreeSet<String>, String> {
    validate(nl, c)?;
    let tile = &db.tiles[site.tile];
    let y = xy(&db.templates[tile.template].sites[site.index].name)?.1;
    let prefix = format!("{}.DSP48.DSP_{y}", tile.name);
    let mut f = BTreeSet::new();
    for key in [
        "AREG_0",
        "BREG_0",
        "ZAREG_2_ACASCREG_1",
        "ZBREG_2_BCASCREG_1",
        "ZIS_CLK_INVERTED",
        "ZIS_CARRYIN_INVERTED",
    ] {
        f.insert(format!("{prefix}.{key}"));
    }
    for key in [
        "ADREG",
        "ALUMODEREG",
        "CARRYINREG",
        "CARRYINSELREG",
        "CREG",
        "DREG",
        "INMODEREG",
        "MREG",
        "OPMODEREG",
        "PREG",
    ] {
        f.insert(format!("{prefix}.Z{key}[0]"));
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
        if pin.dir != Dir::Input || !active_pin(nl.str(pin.name)) {
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
