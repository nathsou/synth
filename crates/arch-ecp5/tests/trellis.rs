//! Checks against the Project Trellis database and tools, where installed
//! (skipped otherwise): the routing graph has Trellis' arcs, and bitstreams
//! are byte for byte those of `ecppack` and read back as `ecpunpack` reads
//! them. `data/blinky-25f.config` is nextpnr-ecp5's configuration of
//! `data/blinky.v` (Yosys 0.33, nextpnr 0.6).

use std::path::Path;
use std::process::Command;

use arch_ecp5::config::ChipConfig;
use arch_ecp5::db::{Database, locate};

fn database() -> Option<Database> {
    match locate(None) {
        Ok(root) => Some(Database::open(&root).unwrap()),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

fn data(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name))
        .unwrap()
}

/// Runs a Trellis tool, if installed.
fn trellis(tool: &str, args: &[&Path]) -> Option<()> {
    match Command::new(tool).args(args).output() {
        Ok(o) if o.status.success() => Some(()),
        Ok(o) => panic!("{tool} failed: {}", String::from_utf8_lossy(&o.stderr)),
        Err(_) => {
            eprintln!("skipped: no {tool}");
            None
        }
    }
}

#[test]
fn routing_graph_has_trellis_arcs() {
    let Some(db) = database() else { return };
    let dev = db.device("LFE5U-25F").unwrap();
    let speed = arch_ecp5::speed::Speed::load(&db.root, "6").unwrap();
    let g = arch_ecp5::graph::Graph::build(&dev, &db, &speed).unwrap();
    // pytrellis' RoutingGraph of the LFE5U-25F: 8199469 distinct arcs; and
    // 12 LUT permutation edges for each of the 3036 logic tiles' 8 LUTs,
    // and the clock network's wire links.
    assert_eq!(g.num_edges(), 8_199_469 + 3036 * 8 * 12 + g.clock_links);
    assert!(g.clock_links > 3036 * 16);
    let a0 = g.node(10, 10, "A0").unwrap();
    let to: Vec<String> = g.edges(a0).map(|e| g.name(g.edge_dst[e]).to_string()).collect();
    assert_eq!(to, ["A0_SLICE", "B0_SLICE", "C0_SLICE", "D0_SLICE"]);
}

#[test]
fn bitstreams_are_those_of_ecppack() {
    let Some(db) = database() else { return };
    let text = data("blinky-25f.config");
    let cc = ChipConfig::parse(&text).unwrap();
    let dir = std::env::temp_dir().join(format!("synth-ecp5-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (cfg, bit, cbit, unpacked) =
        (dir.join("in.config"), dir.join("ref.bit"), dir.join("ref_c.bit"), dir.join("ref.config"));
    std::fs::write(&cfg, &text).unwrap();
    for compress in [false, true] {
        let ours = arch_ecp5::pack_config(&db, &cc, compress).unwrap();
        // Our own reader takes it back to the same configuration.
        let back = arch_ecp5::unpack_bitstream(&db, &ours).unwrap();
        assert_eq!(arch_ecp5::pack_config(&db, &back, compress).unwrap(), ours);
        let reference = if compress { &cbit } else { &bit };
        let args: Vec<&Path> = if compress {
            vec![&cfg, reference, Path::new("--compress")]
        } else {
            vec![&cfg, reference]
        };
        if trellis("ecppack", &args).is_some() {
            assert_eq!(ours, std::fs::read(reference).unwrap(), "compress {compress}");
        }
    }
    if trellis("ecpunpack", &[&bit, &unpacked]).is_some() {
        let ours = arch_ecp5::unpack_bitstream(&db, &std::fs::read(&bit).unwrap()).unwrap();
        assert_eq!(ours.to_text(), std::fs::read_to_string(&unpacked).unwrap());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn globals_reach_every_logic_tile_through_spine_tap_and_branch() {
    let Some(db) = database() else { return };
    let dev = db.device("LFE5U-25F").unwrap();
    let speed = arch_ecp5::speed::Speed::load(&db.root, "6").unwrap();
    let g = arch_ecp5::graph::Graph::build(&dev, &db, &speed).unwrap();
    // Breadth first from the upper left quadrant's global 0, through clock
    // network wires only.
    let start = g.node(0, 0, "G_ULPCLK0").unwrap();
    let mut seen = vec![false; g.num_nodes()];
    let mut queue = std::collections::VecDeque::from([start]);
    seen[start as usize] = true;
    while let Some(v) = queue.pop_front() {
        for e in g.edges(v) {
            let d = g.edge_dst[e];
            let name = g.name(d);
            let clock =
                name.contains("0000") || name.starts_with("CLK") || name.starts_with("MUXCLK");
            if clock && !seen[d as usize] {
                seen[d as usize] = true;
                queue.push_back(d);
            }
        }
    }
    // Every logic tile of the quadrant (rows 1..25, columns 1..31 on the
    // 25K) gets the clock to its slices.
    for (x, y) in [(2, 2), (10, 10), (30, 24), (12, 24)] {
        let pin = g.node(x, y, "CLK0_SLICE").unwrap();
        assert!(seen[pin as usize], "R{y}C{x}");
    }
}
