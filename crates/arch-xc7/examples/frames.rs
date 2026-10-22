//! Assemble FASM or inspect frame data with the native backend.
use std::{fmt::Write, path::Path};
fn main() -> Result<(), String> {
    let a: Vec<_> = std::env::args().skip(1).collect();
    if a.len() != 4 {
        return Err("usage: frames <db/artix7> <part> <input.fasm> <output-prefix>".into());
    }
    let db = arch_xc7::device::Device::geometry(Path::new(&a[0]), &a[1])?;
    let source = std::fs::read_to_string(&a[2]).map_err(|e| e.to_string())?;
    let features = arch_xc7::fasm::parse(&source)?;
    let frames = arch_xc7::fasm::Assembler::new(&db, Path::new(&a[0])).assemble(&features)?;
    let mut text = String::new();
    for (&addr, frame) in &frames {
        write!(text, "0x{addr:08x} ").unwrap();
        for (i, w) in frame.iter().enumerate() {
            if i > 0 {
                text.push(',');
            }
            write!(text, "0x{w:08x}").unwrap();
        }
        text.push('\n');
    }
    std::fs::write(format!("{}.frm", a[3]), text).map_err(|e| e.to_string())?;
    for crc in [true, false] {
        let raw = arch_xc7::bitstream::encode(&db.part, &frames, crc)?;
        arch_xc7::bitstream::decode(&db.part, &raw)?;
        std::fs::write(format!("{}.{}.bin", a[3], if crc { "crc" } else { "xray" }), raw)
            .map_err(|e| e.to_string())?;
    }
    println!("{} features, {} frames", features.len(), frames.len());
    Ok(())
}
