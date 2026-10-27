//! The configuration of an empty design on each device (`data/base`),
//! which every bitstream starts from.

use crate::config::ChipConfig;

/// The base configuration of `device`.
pub fn config(device: &str) -> Result<ChipConfig, String> {
    let text = match device {
        // The 12K is a 25K die.
        "LFE5U-25F" | "LFE5U-12F" => include_str!("../data/base/LFE5U-25F.config"),
        "LFE5U-45F" => include_str!("../data/base/LFE5U-45F.config"),
        "LFE5U-85F" => include_str!("../data/base/LFE5U-85F.config"),
        "LFE5UM-25F" => include_str!("../data/base/LFE5UM-25F.config"),
        "LFE5UM-45F" => include_str!("../data/base/LFE5UM-45F.config"),
        "LFE5UM-85F" => include_str!("../data/base/LFE5UM-85F.config"),
        "LFE5UM5G-25F" => include_str!("../data/base/LFE5UM5G-25F.config"),
        "LFE5UM5G-45F" => include_str!("../data/base/LFE5UM5G-45F.config"),
        "LFE5UM5G-85F" => include_str!("../data/base/LFE5UM5G-85F.config"),
        _ => return Err(format!("no base configuration for `{device}`")),
    };
    let mut cc = ChipConfig::parse(text)?;
    cc.device = device.to_string();
    Ok(cc)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_device_has_a_base_configuration() {
        for d in ["LFE5U-12F", "LFE5U-25F", "LFE5UM-85F", "LFE5UM5G-45F"] {
            let cc = super::config(d).unwrap();
            assert_eq!(cc.device, d);
            assert!(!cc.tiles.is_empty());
        }
    }
}
