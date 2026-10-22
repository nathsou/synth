//! RAMB18E1 TDP and 36-bit SDP legalization, site pins and configuration.
use crate::{
    device::Device,
    pack::{Packed, param},
    place::Site,
};
use std::collections::BTreeSet;
use synth_netlist::{Bit, CellId, Dir, NetId, Netlist};
pub fn validate(nl: &Netlist, c: CellId) -> Result<(), String> {
    let mode = nl.param(c, "RAM_MODE").and_then(|p| p.as_str()).unwrap_or("TDP");
    if mode == "SDP" {
        // The native mapper's symmetric SDP mode. Its output register spans
        // both halves of the word, so both are registered or neither.
        if param(nl, c, "READ_WIDTH_A", 0) != 36
            || param(nl, c, "WRITE_WIDTH_B", 0) != 36
            || ["READ_WIDTH_B", "WRITE_WIDTH_A"].iter().any(|p| param(nl, c, p, 0) != 0)
            || param(nl, c, "DOA_REG", 0) != param(nl, c, "DOB_REG", 0)
            || param(nl, c, "DOA_REG", 0) > 1
        {
            return Err(
                "physical SDP RAMB18 requires 36-bit read/write ports and one output register \
                        setting"
                    .into(),
            );
        }
        return Ok(());
    } else if mode != "TDP" {
        return Err("invalid RAMB18 RAM_MODE".into());
    }
    for p in ["READ_WIDTH_A", "READ_WIDTH_B", "WRITE_WIDTH_A", "WRITE_WIDTH_B"] {
        if !matches!(param(nl, c, p, 0), 0 | 1 | 2 | 4 | 9 | 18) {
            return Err(format!("unsupported RAMB18 {p}"));
        }
    }
    for p in ["DOA_REG", "DOB_REG"] {
        if param(nl, c, p, 0) > 1 {
            return Err(format!("invalid RAMB18 {p}"));
        }
    }
    Ok(())
}
pub fn inputs(nl: &Netlist, c: CellId, p: &Packed) -> Result<Vec<(String, NetId)>, String> {
    let zero = *p.constant_luts.keys().find(|n| !p.constants[n]).ok_or("no ground")?;
    let one = *p.constant_luts.keys().find(|n| p.constants[n]).ok_or("no VCC")?;
    let mut pins = Vec::new();
    for pin in &nl.cells[c].pins {
        if pin.dir != Dir::Input {
            continue;
        }
        let Some(n) = pin.net else { continue };
        let name = nl.str(pin.name);
        if name.starts_with("WEA[") || name.starts_with("WEBWE[") {
            continue;
        }
        pins.push((name.replace(['[', ']'], ""), n));
    }
    for (port, count, base) in [("A", 4, "WEA"), ("B", 8, "WEBWE")] {
        let width = param(nl, c, &format!("WRITE_WIDTH_{port}"), 0) as usize;
        for i in 0..count {
            let n = if width == 0 || port == "B" && width != 36 && i >= 4 {
                zero
            } else {
                // SDP's four byte enables each drive two physical WEBWE pins.
                let logical = if port == "A" || width == 36 { i / 2 } else { i };
                let used = (width / 9).max(1);
                nl.pin_net(c, &format!("{base}[{}]", logical % used)).unwrap_or(zero)
            };
            pins.push((format!("{base}{i}"), n));
        }
    }
    for pin in ["ADDRATIEHIGH0", "ADDRATIEHIGH1", "ADDRBTIEHIGH0", "ADDRBTIEHIGH1"] {
        pins.push((pin.into(), one));
    }
    // Output register clocks follow their port clocks, as the vendor tools
    // tie them, whether or not the register is used.
    for (reg, clk) in [("REGCLKARDRCLK", "CLKARDCLK"), ("REGCLKB", "CLKBWRCLK")] {
        if let Some(n) = nl.pin_net(c, clk).filter(|&n| nl.nets[n].constant.is_none()) {
            pins.push((reg.into(), n));
        }
    }
    Ok(pins)
}
pub fn features(
    db: &Device,
    nl: &Netlist,
    c: CellId,
    site: &Site,
) -> Result<BTreeSet<String>, String> {
    validate(nl, c)?;
    let t = &db.tiles[site.tile];
    let y = crate::device::xy(&db.templates[t.template].sites[site.index].name)?.1;
    let prefix = format!("{}.RAMB18_Y{y}", t.name);
    let mut f = BTreeSet::new();
    let mut add = |s: String| {
        f.insert(format!("{prefix}.{s}"));
    };
    add("IN_USE".into());
    if param(nl, c, "READ_WIDTH_A", 0) == 36 {
        // SDP spans both data sides even though the opposite logical widths
        // are zero. Width-1 defaults would disable half the word. The upper
        // site's A read marker is asymmetric in the Series-7 encoding.
        for p in [
            "SDP_READ_WIDTH_36",
            "SDP_WRITE_WIDTH_36",
            "READ_WIDTH_B_18",
            "WRITE_WIDTH_A_18",
            "WRITE_WIDTH_B_18",
        ] {
            add(p.into());
        }
        if y == 1 {
            add("READ_WIDTH_A_18".into());
        }
    } else {
        for p in ["READ_WIDTH_A", "READ_WIDTH_B", "WRITE_WIDTH_A", "WRITE_WIDTH_B"] {
            add(format!("{p}_{}", param(nl, c, p, 0).max(1)));
        }
    }
    for p in ["DOA_REG", "DOB_REG"] {
        if param(nl, c, p, 0) != 0 {
            add(p.into());
        }
    }
    for pin in [
        "CLKARDCLK",
        "CLKBWRCLK",
        "ENARDEN",
        "ENBWREN",
        "RSTRAMARSTRAM",
        "RSTRAMB",
        "RSTREGARSTREG",
        "RSTREGB",
    ] {
        if pin == "RSTREGARSTREG" && param(nl, c, "DOA_REG", 0) == 0
            || pin == "RSTREGB" && param(nl, c, "DOB_REG", 0) == 0
        {
            continue;
        }
        if param(nl, c, &format!("IS_{pin}_INVERTED"), 0) == 0 {
            add(format!("ZINV_{pin}"));
        }
    }
    for (reg, clk) in [("REGCLKARDRCLK", "CLKARDCLK"), ("REGCLKB", "CLKBWRCLK")] {
        if param(nl, c, &format!("IS_{clk}_INVERTED"), 0) == 0 {
            add(format!("ZINV_{reg}"));
        }
    }
    for ab in ["A", "B"] {
        let priority = nl
            .param(c, &format!("RSTREG_PRIORITY_{ab}"))
            .and_then(|v| v.as_str())
            .unwrap_or("RSTREG");
        if !matches!(priority, "RSTREG" | "REGCE") {
            return Err("invalid reset priority".into());
        }
        add(format!("RSTREG_PRIORITY_{ab}_{priority}"));
        let mode = nl
            .param(c, &format!("WRITE_MODE_{ab}"))
            .and_then(|v| v.as_str())
            .unwrap_or("WRITE_FIRST");
        if !matches!(mode, "READ_FIRST" | "WRITE_FIRST" | "NO_CHANGE") {
            return Err("invalid BRAM write mode".into());
        }
        if mode != "WRITE_FIRST" {
            add(format!("WRITE_MODE_{ab}_{mode}"));
        }
        for par in ["INIT", "SRVAL"] {
            let init = param(nl, c, &format!("{par}_{ab}"), 0);
            for bit in 0..18 {
                if init >> bit & 1 == 0 {
                    add(format!("Z{par}_{ab}[{bit}]"));
                }
            }
        }
    }
    if nl.param(c, "RDADDR_COLLISION_HWCONFIG").and_then(|v| v.as_str()) == Some("PERFORMANCE") {
        add("RDADDR_COLLISION_HWCONFIG_PERFORMANCE".into());
    }
    for mode in ["", "P"] {
        for i in 0..if mode.is_empty() { 64 } else { 8 } {
            let key = format!("INIT{mode}_{i:02X}");
            if let Some(v) = nl.param(c, &key) {
                for (bit, value) in v.to_bits(256).iter().enumerate() {
                    if *value == Bit::One {
                        add(format!("{key}[{bit}]"));
                    }
                }
            }
        }
    }
    for key in ["ZALMOST_EMPTY_OFFSET", "ZALMOST_FULL_OFFSET"] {
        for bit in 0..13 {
            f.insert(format!("{}.{key}[{bit}]", t.name));
        }
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synth_netlist::Param;

    #[test]
    fn sdp_routes_all_four_byte_enables_to_paired_site_pins() {
        let mut nl = Netlist::new();
        let c = nl.add_cell("ram", "RAMB18E1");
        nl.set_param(c, "RAM_MODE", Param::Str("SDP".into()));
        nl.set_param(c, "READ_WIDTH_A", Param::Int(36));
        nl.set_param(c, "WRITE_WIDTH_B", Param::Int(36));
        let enables: Vec<_> = (0..4)
            .map(|i| {
                let n = nl.add_net(&format!("enable{i}"));
                nl.add_pin(c, &format!("WEBWE[{i}]"), Dir::Input, Some(n)).unwrap();
                n
            })
            .collect();
        validate(&nl, c).unwrap();
        let packed = crate::pack::pack(&mut nl).unwrap();
        let pins: std::collections::BTreeMap<_, _> =
            inputs(&nl, c, &packed).unwrap().into_iter().collect();
        for (i, enable) in enables.iter().enumerate() {
            assert_eq!(pins[&format!("WEBWE{}", 2 * i)], *enable);
            assert_eq!(pins[&format!("WEBWE{}", 2 * i + 1)], *enable);
        }
        for i in 0..4 {
            assert!(!packed.constants[&pins[&format!("WEA{i}")]]);
        }
        for (key, value) in [("DOA_REG", 1), ("READ_WIDTH_B", 18), ("WRITE_WIDTH_B", 18)] {
            let mut invalid = nl.clone();
            invalid.set_param(c, key, Param::Int(value));
            assert!(validate(&invalid, c).is_err(), "{key}");
        }
    }
}
