//! Every example through the playground's whole flow.

use synth_core::Json;

fn request(ex: &Json) -> Json {
    Json::obj()
        .with("files", ex.at(&["files"]).clone())
        .with("pcf", ex.at(&["pcf"]).clone())
        .with("part", ex.at(&["part"]).clone())
        .with("package", ex.at(&["package"]).clone())
        // Less placement effort: the test checks the data, not its quality.
        .with("effort", 0.3)
}

#[test]
fn examples_run_end_to_end() {
    let exs = synth_playground::examples();
    for ex in exs.as_array() {
        let id = ex.at(&["id"]).as_str().unwrap();
        let t = std::time::Instant::now();
        let out = Json::parse(&synth_playground::handle("run", &request(ex).dump())).unwrap();
        let size = out.dump().len();
        assert_eq!(out.at(&["ok"]).as_bool(), Some(true), "{id}: {}", out.at(&["error"]).dump());
        let stages: Vec<&str> = out
            .at(&["stages"])
            .as_array()
            .iter()
            .map(|s| s.at(&["id"]).as_str().unwrap())
            .collect();
        assert_eq!(stages.first(), Some(&"elaborated"), "{id}: {stages:?}");
        assert_eq!(stages.last(), Some(&"packed"), "{id}: {stages:?}");
        for s in out.at(&["stages"]).as_array() {
            let g = s.at(&["graph"]);
            let n = g.at(&["nodes"]).as_array().len();
            for e in g.at(&["edges"]).as_array() {
                for k in ["s", "d"] {
                    assert!((e.at(&[k]).as_i64().unwrap() as usize) < n, "{id}: dangling edge");
                }
            }
        }
        let chip = out.at(&["chip"]);
        let nodes = chip.at(&["nodes"]).as_array().len();
        assert!(!chip.at(&["cells"]).as_array().is_empty());
        for net in chip.at(&["nets"]).as_array() {
            for p in net.at(&["pips"]).as_array() {
                assert!((p.as_array()[0].as_i64().unwrap() as usize) < nodes);
            }
        }
        let bits = out.at(&["bitstream"]);
        assert!(bits.at(&["set_bits"]).as_i64().unwrap() > 0);
        let tiles = bits.at(&["tiles"]).as_array();
        // Every set bit of a logic tile is explained by a feature.
        for tile in tiles.iter().filter(|t| t.at(&["k"]).as_i64() == Some(2)) {
            let set = tile.at(&["set"]).as_array().len();
            let explained: usize =
                tile.at(&["f"]).as_array().iter().map(|f| f.at(&["b"]).as_array().len()).sum();
            assert_eq!(set, explained, "{id}: tile {}", tile.dump());
        }
        assert!(
            out.at(&["timing", "clocks"])
                .as_array()
                .iter()
                .all(|c| !c.at(&["path"]).as_array().is_empty())
        );
        eprintln!(
            "{id}: {:.2}s, {} KiB, stages {stages:?}",
            t.elapsed().as_secs_f64(),
            size / 1024
        );
    }
}

#[test]
fn errors_come_back_as_diagnostics() {
    let req = Json::obj().with(
        "files",
        vec![
            Json::obj()
                .with("name", "bad.weft")
                .with("text", "module top(in a: bool, out y: bool) { y = b; }"),
        ],
    );
    let out = Json::parse(&synth_playground::handle("run", &req.dump())).unwrap();
    assert_eq!(out.at(&["ok"]).as_bool(), Some(false));
    let d = &out.at(&["diagnostics"]).as_array()[0];
    assert_eq!(d.at(&["severity"]).as_str(), Some("error"));
    assert_eq!(d.at(&["at", "line"]).as_i64(), Some(1));
    let a = Json::parse(&synth_playground::handle("analyze", &req.dump())).unwrap();
    assert!(!a.at(&["diagnostics"]).as_array().is_empty());
    assert!(a.at(&["refs"]).as_array().iter().any(|r| r.at(&["kind"]).as_str() == Some("Port")));
    let dev = Json::parse(&synth_playground::handle("devices", "")).unwrap();
    assert!(dev.as_array().iter().any(|d| d.at(&["part"]).as_str() == Some("hx8k")));
}
