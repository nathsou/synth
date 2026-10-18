//! Versioned implementation checkpoints. These are optimization hints, never
//! a substitute for elaborating the current design or checking physical legality.

use std::hash::Hasher;

use synth_core::bin::{BinReader, BinWriter};
use synth_core::{FxHashMap, FxHasher};
use synth_netlist::{CellId, Netlist};

use crate::arch::{Arch, BelId};
use crate::flow::FlowOptions;
use crate::place::Placement;
use crate::route::{Routing, sink_node};

const MAGIC: &[u8; 8] = b"SYNTHinc";
const VERSION: u32 = 1;

#[derive(Clone, Debug)]
struct Cell {
    name: String,
    signature: String,
    bel: u32,
}

#[derive(Clone, Debug)]
struct Net {
    source: u32,
    sinks: Vec<u32>,
    edges: Vec<u32>,
}

/// A portable snapshot of placement and routing, independent of allocation IDs.
/// Candidate routes are validated by the router before occupying any wires.
#[derive(Clone, Debug)]
pub struct Checkpoint {
    context: String,
    graph: u64,
    cells: Vec<Cell>,
    nets: Vec<Net>,
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write(bytes);
    h.finish()
}

fn context(opts: &FlowOptions) -> String {
    let mut clocks: Vec<_> = opts.clocks.nets.iter().collect();
    clocks.sort_by(|a, b| a.0.cmp(&b.0));
    format!(
        "{} {} {} {} {} {} {:?} {:?} {}",
        env!("CARGO_PKG_VERSION"),
        opts.part,
        opts.package,
        opts.seed,
        opts.place_effort,
        opts.timing_weight,
        opts.clocks.default_mhz,
        clocks,
        opts.route_timing_rounds
    )
}

// Edge and BEL indices must describe exactly the same architecture. Include
// configuration-pip identities as well as connectivity, not merely graph sizes.
fn graph_key(arch: &Arch) -> u64 {
    let mut h = FxHasher::default();
    for &v in &arch.graph.offsets {
        h.write_u32(v);
    }
    for (&d, p) in arch.graph.edge_dst.iter().zip(&arch.graph.edge_pip) {
        h.write_u32(d);
        h.write_u16(p.tile);
        h.write_u16(p.tpip);
    }
    for b in &arch.bels {
        h.write(format!("{b:?}").as_bytes());
    }
    h.finish()
}

fn signature(nl: &Netlist, c: CellId) -> String {
    let cell = &nl.cells[c];
    let mut params: Vec<_> = cell
        .params
        .iter()
        .filter_map(|(k, v)| {
            let k = nl.str(*k);
            // Configuration contents alone do not require a different BEL. The
            // current contents are always used when generating the new bitstream.
            if (nl.cell_kind(c) == crate::pack::LC && k == "LUT_INIT")
                || (nl.cell_kind(c) == crate::pack::RAM && k.starts_with("INIT_"))
            {
                None
            } else {
                Some((k, v.to_string()))
            }
        })
        .collect();
    params.sort();
    let mut pins: Vec<_> =
        cell.pins.iter().map(|p| (nl.str(p.name), format!("{:?}", p.dir))).collect();
    pins.sort();
    format!("{} {params:?} {pins:?}", nl.cell_kind(c))
}

impl Checkpoint {
    pub(crate) fn compatible(&self, arch: &Arch, opts: &FlowOptions) -> bool {
        self.context == context(opts) && self.graph == graph_key(arch)
    }

    pub(crate) fn placement(&self, nl: &Netlist) -> Placement {
        let mut placement = Placement::default();
        for old in &self.cells {
            if let Some(c) = nl.cell_by_name(&old.name)
                && nl.cells[c].alive
                && signature(nl, c) == old.signature
            {
                placement.cell_bel.insert(c, BelId::from_raw(old.bel));
            }
        }
        placement
    }

    pub(crate) fn routes(&self) -> FxHashMap<(u32, Vec<u32>), Vec<u32>> {
        self.nets.iter().map(|n| ((n.source, n.sinks.clone()), n.edges.clone())).collect()
    }

    pub(crate) fn capture(
        arch: &Arch,
        nl: &Netlist,
        placement: &Placement,
        routing: &Routing,
        opts: &FlowOptions,
    ) -> Result<Self, String> {
        let mut cells: Vec<_> = placement
            .cell_bel
            .iter()
            .map(|(&c, &b)| Cell {
                name: nl.str(nl.cells[c].name).into(),
                signature: signature(nl, c),
                bel: b.raw(),
            })
            .collect();
        cells.sort_by(|a, b| a.name.cmp(&b.name));
        let mut nets = Vec::new();
        for (&n, edges) in &routing.net_edges {
            let source = routing.net_source[&n].raw();
            let mut sinks = Vec::new();
            for p in &nl.nets[n].users {
                let name = nl.str(nl.cells[p.cell].pins[p.pin as usize].name);
                let node = sink_node(arch, nl, placement, p.cell, name)?.raw();
                if node != source {
                    sinks.push(node);
                }
            }
            sinks.sort_unstable();
            sinks.dedup();
            nets.push(Net { source, sinks, edges: edges.clone() });
        }
        nets.sort_by(|a, b| (a.source, &a.sinks).cmp(&(b.source, &b.sinks)));
        Ok(Self { context: context(opts), graph: graph_key(arch), cells, nets })
    }

    /// Encode deterministically. The checksum detects accidental corruption;
    /// physical validation is still required after decoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = BinWriter::new();
        w.raw(MAGIC);
        w.u32_fixed(VERSION);
        w.str(&self.context);
        w.uvar(self.graph);
        w.seq(&self.cells, |w, c| {
            w.str(&c.name);
            w.str(&c.signature);
            w.uvar(c.bel as u64);
        });
        w.seq(&self.nets, |w, n| {
            w.uvar(n.source as u64);
            w.seq(&n.sinks, |w, &v| w.uvar(v as u64));
            w.seq(&n.edges, |w, &v| w.uvar(v as u64));
        });
        let mut data = w.into_bytes();
        data.extend_from_slice(&checksum(&data).to_le_bytes());
        data
    }

    /// Reject unknown formats, truncated files and corrupted payloads.
    pub fn decode(data: &[u8]) -> Result<Self, String> {
        let end = data.len().checked_sub(8).ok_or("truncated checkpoint")?;
        let (data, sum) = data.split_at(end);
        if checksum(data) != u64::from_le_bytes(sum.try_into().unwrap()) {
            return Err("checkpoint checksum mismatch".into());
        }
        let mut r = BinReader::new(data);
        let decode = |r: &mut BinReader<'_>| -> Result<Self, synth_core::bin::BinError> {
            if r.raw(8)? != MAGIC || r.u32_fixed()? != VERSION {
                return Err(synth_core::bin::BinError {
                    offset: 0,
                    message: "unsupported checkpoint format",
                });
            }
            Ok(Self {
                context: r.string()?,
                graph: r.uvar()?,
                cells: r.seq(|r| {
                    Ok(Cell { name: r.string()?, signature: r.string()?, bel: r.u32()? })
                })?,
                nets: r.seq(|r| {
                    Ok(Net {
                        source: r.u32()?,
                        sinks: r.seq(|r| r.u32())?,
                        edges: r.seq(|r| r.u32())?,
                    })
                })?,
            })
        };
        let result = decode(&mut r).map_err(|e| e.to_string())?;
        if !r.at_end() {
            return Err("trailing checkpoint data".into());
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::implement_incremental;
    use synth_netlist::{Dir, INPUT_PORT, OUTPUT_PORT, Param};

    fn options() -> FlowOptions {
        FlowOptions {
            part: crate::arch::Part::Hx1k,
            package: "vq100".into(),
            seed: 1,
            place_effort: 0.1,
            timing_weight: 0.5,
            clocks: Default::default(),
            route_timing_rounds: 0,
        }
    }

    // Two independently registered LUTs. Optional dead allocations verify that
    // persistence never assumes CellId/NetId stability across compilations.
    fn design(init: i64, extra: bool, shift_ids: bool) -> Netlist {
        let mut nl = Netlist::new();
        if shift_ids {
            let c = nl.add_cell("dead", "SB_LUT4");
            nl.remove_cell(c);
            nl.add_net("unused");
        }
        let mut inputs = Vec::new();
        for name in ["clk", "a", "b"] {
            let n = nl.add_net(name);
            let c = nl.add_cell(name, INPUT_PORT);
            nl.add_pin(c, "PAD", Dir::Output, Some(n)).unwrap();
            inputs.push(n);
        }
        for i in 0..if extra { 3 } else { 2 } {
            let d = nl.add_net(&format!("d{i}"));
            let q = nl.add_net(&format!("q{i}"));
            let lut = nl.add_cell(&format!("lut{i}"), "SB_LUT4");
            for k in 0..4 {
                nl.add_pin(lut, &format!("I{k}"), Dir::Input, (k < 2).then(|| inputs[k + 1]))
                    .unwrap();
            }
            nl.add_pin(lut, "O", Dir::Output, Some(d)).unwrap();
            nl.set_param(lut, "LUT_INIT", Param::Int(if i == 0 { init } else { 0x8888 }));
            let ff = nl.add_cell(&format!("ff{i}"), "SB_DFF");
            nl.add_pin(ff, "D", Dir::Input, Some(d)).unwrap();
            nl.add_pin(ff, "C", Dir::Input, Some(inputs[0])).unwrap();
            nl.add_pin(ff, "Q", Dir::Output, Some(q)).unwrap();
            let port = nl.add_cell(&format!("y{i}"), OUTPUT_PORT);
            nl.add_pin(port, "PAD", Dir::Input, Some(q)).unwrap();
        }
        nl.check().unwrap();
        nl
    }

    #[test]
    fn no_op_and_lut_edit_reuse_physical_implementation() {
        let opts = options();
        let pins = Default::default();
        let first =
            implement_incremental(design(0x6666, false, false), &pins, &opts, None).unwrap();
        let bytes = first.checkpoint.as_ref().unwrap().encode();
        let old = Checkpoint::decode(&bytes).unwrap();
        assert_eq!(old.encode(), bytes);
        let same =
            implement_incremental(design(0x6666, false, true), &pins, &opts, Some(&old)).unwrap();
        assert_eq!(first.config, same.config);
        assert!(same.log.iter().any(|l| l.contains("reused 2/2 logic/RAM cells")));
        assert!(!same.log.iter().any(|l| l.contains("using fresh")));
        assert_eq!(old.nets.len(), same.checkpoint.as_ref().unwrap().nets.len());
        for (a, b) in old.nets.iter().zip(&same.checkpoint.as_ref().unwrap().nets) {
            assert_eq!(a.edges, b.edges);
        }
        let edited =
            implement_incremental(design(0x9999, false, true), &pins, &opts, Some(&old)).unwrap();
        assert_ne!(first.config, edited.config);
        assert!(edited.log.iter().any(|l| l.contains("reused 2/2 logic/RAM cells")));
        for (a, b) in old.nets.iter().zip(&edited.checkpoint.as_ref().unwrap().nets) {
            assert_eq!(a.edges, b.edges);
        }
        let third = implement_incremental(
            design(0x9999, false, false),
            &pins,
            &opts,
            edited.checkpoint.as_ref(),
        )
        .unwrap();
        assert_eq!(
            edited.config, third.config,
            "LUT permutations must not accumulate across saves"
        );
    }

    #[test]
    fn local_addition_and_removal_repair_then_stabilize() {
        let opts = options();
        let pins = Default::default();
        let first =
            implement_incremental(design(0x6666, false, false), &pins, &opts, None).unwrap();
        let added = implement_incremental(
            design(0x6666, true, true),
            &pins,
            &opts,
            first.checkpoint.as_ref(),
        )
        .unwrap();
        assert!(added.log.iter().any(|l| l.contains("reused 2/3 logic/RAM cells")));
        assert!(!added.log.iter().any(|l| l.contains("using fresh")));
        let same = implement_incremental(
            design(0x6666, true, false),
            &pins,
            &opts,
            added.checkpoint.as_ref(),
        )
        .unwrap();
        assert_eq!(same.config, added.config);
        let removed = implement_incremental(
            design(0x6666, false, false),
            &pins,
            &opts,
            added.checkpoint.as_ref(),
        )
        .unwrap();
        assert!(removed.log.iter().any(|l| l.contains("reused 2/2 logic/RAM cells")));
        assert!(!removed.log.iter().any(|l| l.contains("using fresh")));
    }

    #[test]
    fn corrupt_incompatible_and_invalid_hints_are_safe() {
        let opts = options();
        let pins = Default::default();
        let first =
            implement_incremental(design(0x6666, false, false), &pins, &opts, None).unwrap();
        let old = first.checkpoint.unwrap();
        let mut data = old.encode();
        for n in [0, 7, 12, data.len() - 1] {
            assert!(Checkpoint::decode(&data[..n]).is_err());
        }
        data[20] ^= 1;
        assert!(Checkpoint::decode(&data).is_err());
        let mut different = opts.clone();
        different.seed += 1;
        let rebuilt =
            implement_incremental(design(0x6666, false, false), &pins, &different, Some(&old))
                .unwrap();
        let fresh =
            implement_incremental(design(0x6666, false, false), &pins, &different, None).unwrap();
        assert_eq!(rebuilt.config, fresh.config);
        assert!(rebuilt.log.iter().any(|l| l.contains("incompatible")));
        let mut invalid = old.clone();
        for c in &mut invalid.cells {
            c.bel = u32::MAX;
        }
        for n in &mut invalid.nets {
            n.edges = vec![u32::MAX];
        }
        let repaired =
            implement_incremental(design(0x6666, false, false), &pins, &opts, Some(&invalid))
                .unwrap();
        assert!(repaired.log.iter().any(|l| l.contains("reused 0/2 logic/RAM cells")));
        assert!(repaired.timing_met);

        // A compatible hint that cannot meet the requested timing must retry
        // the normal flow, and retain its normal timing-failure result.
        different.clocks.default_mhz = Some(100000.0);
        let mut timed = design(0x6666, false, false);
        let lut = timed.cell_by_name("lut0").unwrap();
        let ff = timed.cell_by_name("ff0").unwrap();
        let q = timed.pin_net(ff, "Q").unwrap();
        let input = synth_netlist::PinRef { cell: lut, pin: 0 };
        timed.disconnect(input);
        timed.connect(input, q).unwrap();
        let mut too_slow =
            implement_incremental(timed.clone(), &pins, &opts, None).unwrap().checkpoint.unwrap();
        too_slow.context = context(&different);
        let result = implement_incremental(timed, &pins, &different, Some(&too_slow)).unwrap();
        assert!(!result.timing_met);
        assert!(result.log.iter().any(|l| l.contains("incremental result misses timing targets")));
    }
}
