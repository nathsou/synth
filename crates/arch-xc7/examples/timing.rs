//! Audit the upstream SDF grammar and timing coverage.
fn main() -> Result<(), String> {
    let root = std::env::args().nth(1).ok_or("usage: timing <database/artix7/timings>")?;
    let mut files = 0;
    let mut arcs = 0;
    for e in std::fs::read_dir(&root).map_err(|e| e.to_string())? {
        let p = e.map_err(|e| e.to_string())?.path();
        if p.extension().is_none_or(|s| s != "sdf") {
            continue;
        }
        let s = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
        let t = arch_xc7::timing::Sdf::parse(&s).map_err(|e| format!("{}: {e}", p.display()))?;
        files += 1;
        arcs += t.cells.values().map(|c| c.arc_count()).sum::<usize>();
    }
    println!("{files} timing files, {arcs} arcs");
    Ok(())
}
