//! Native physical-flow bring-up driver.
fn main() -> Result<(), String> {
    let a: Vec<_> = std::env::args().skip(1).collect();
    if a.len() != 5 {
        return Err(
            "usage: build <database/artix7> <part> <source.v> <constraints.xdc> <out-prefix>"
                .into(),
        );
    }
    let mut sm = synth_core::diag::SourceMap::new();
    let file = sm.add(a[2].clone(), std::fs::read_to_string(&a[2]).map_err(|e| e.to_string())?);
    let (ast, diags) = synth_verilog::parse_files(&mut sm, &[file], &Default::default());
    if diags.has_errors() {
        return Err(diags.render(&sm));
    }
    let (module, diags) =
        synth_elab::elaborate(&ast, &sm, &Default::default(), &synth_elab::NoBlackBoxes);
    if diags.has_errors() {
        return Err(diags.render(&sm));
    }
    let (nl, _) = arch_xc7::synth::synthesize(module.ok_or("no module")?)?;
    let xdc = arch_xc7::xdc::Constraints::parse(
        &std::fs::read_to_string(&a[3]).map_err(|e| e.to_string())?,
    )?;
    let opts = arch_xc7::flow::FlowOptions { part: a[1].clone(), seed: 1, effort: 1.0, tries: 1 };
    let result = arch_xc7::flow::implement(nl, std::path::Path::new(&a[0]), &xdc, &opts)?;
    for l in result.log {
        eprintln!("info: {l}");
    }
    std::fs::write(format!("{}.bin", a[4]), result.bitstream).map_err(|e| e.to_string())?;
    std::fs::write(format!("{}.fasm", a[4]), arch_xc7::fasm::text(&result.features))
        .map_err(|e| e.to_string())?;
    Ok(())
}
