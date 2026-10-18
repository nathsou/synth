//! Inspect native device import; no external tools are invoked.
fn main() -> Result<(), String> {
    let a: Vec<_> = std::env::args().skip(1).collect();
    if a.len() != 2 {
        return Err("usage: device <database/artix7> <part>".into());
    }
    let now = std::time::Instant::now();
    let d = arch_xc7::device::Device::load(std::path::Path::new(&a[0]), &a[1])?;
    let wires: usize = d.tiles.iter().map(|t| t.nodes.len()).sum();
    let pips: usize = d.tiles.iter().map(|t| d.templates[t.template].pips.len()).sum();
    let sites: usize = (0..d.tiles.len())
        .map(|i| d.sites(i).map(|s| s.len()))
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .sum();
    println!(
        "{}: {} frames, {} tiles, {} templates, {sites} sites, {wires} wires, {} nodes, {pips} pips, {:.2}s",
        d.part.name,
        d.part.frames.len(),
        d.tiles.len(),
        d.templates.len(),
        d.node_x.len(),
        now.elapsed().as_secs_f64()
    );
    Ok(())
}
