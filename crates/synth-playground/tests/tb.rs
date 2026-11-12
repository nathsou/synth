//! Testbench sessions: a design driven cycle by cycle and read back.

use synth_core::Json;

fn call(method: &str, body: Json) -> Json {
    Json::parse(&synth_playground::handle(method, &body.dump())).unwrap()
}

fn files(text: &str) -> Json {
    Json::Arr(vec![Json::obj().with("name", "top.weft").with("text", text)])
}

const COUNTER: &str = "
module counter(in clk: clock, in enable: bool, in clear: bool, out count: uint<4>, out wrapped: bool) {
    reg value: uint<4> = 0;
    if clear { value := 0; } else if enable { value := value +% 1; }
    count = value;
    wrapped = enable && value == 15;
}
";

#[test]
fn a_counter_steps_and_reads() {
    let s = call("tb_start", Json::obj().with("files", files(COUNTER)));
    assert_eq!(s.at(&["ok"]).as_bool(), Some(true), "{}", s.dump());
    assert_eq!(s.at(&["clocks"]).dump(), r#"["clk"]"#);
    let ports: Vec<&str> =
        s.at(&["ports"]).as_array().iter().map(|p| p.at(&["name"]).as_str().unwrap()).collect();
    assert_eq!(ports, ["clk", "enable", "clear", "count", "wrapped"]);
    let id = s.at(&["id"]).clone();
    let ops = Json::Arr(vec![
        Json::obj().with("set", Json::obj().with("enable", true)),
        Json::obj().with("step", 15.0),
        Json::obj().with("read", Json::Arr(vec!["count".into(), "wrapped".into()])),
        Json::obj().with("step", 2.0).with("trace", Json::Arr(vec!["count".into()])),
    ]);
    let r = call("tb_run", Json::obj().with("id", id.clone()).with("ops", ops));
    assert_eq!(r.at(&["ok"]).as_bool(), Some(true), "{}", r.dump());
    assert_eq!(r.at(&["cycle"]).as_i64(), Some(17));
    assert_eq!(
        r.at(&["reads"]).dump(),
        r#"[{"cycle":15,"values":{"count":15,"wrapped":1}},{"cycle":16,"values":{"count":0}},{"cycle":17,"values":{"count":1}}]"#
    );
    // A failing operation stops the run and names the problem.
    let bad = Json::Arr(vec![Json::obj().with("set", Json::obj().with("nope", 1.0))]);
    let r = call("tb_run", Json::obj().with("id", id.clone()).with("ops", bad));
    assert_eq!(r.at(&["ok"]).as_bool(), Some(false));
    assert!(r.at(&["error"]).as_str().unwrap().contains("nope"));
    // Back to power-up.
    let again = Json::Arr(vec![
        Json::obj().with("restart", true),
        Json::obj().with("read", Json::Arr(vec!["count".into()])),
        Json::obj().with("set", Json::obj().with("enable", true)),
        Json::obj().with("step", 2.0),
        Json::obj().with("read", Json::Arr(vec!["count".into()])),
    ]);
    let r = call("tb_run", Json::obj().with("id", id.clone()).with("ops", again));
    assert_eq!(
        r.at(&["reads"]).dump(),
        r#"[{"cycle":0,"values":{"count":0}},{"cycle":2,"values":{"count":2}}]"#
    );
    call("tb_stop", Json::obj().with("id", id.clone()));
    let r = call("tb_run", Json::obj().with("id", id).with("ops", Json::Arr(vec![])));
    assert_eq!(r.at(&["ok"]).as_bool(), Some(false));
}

#[test]
fn combinational_designs_and_memories() {
    let adder = "module add(in a: uint<8>, in b: uint<8>, out s: uint<9>) { s = a + b; }";
    let s = call("tb_start", Json::obj().with("files", files(adder)));
    assert_eq!(s.at(&["ok"]).as_bool(), Some(true), "{}", s.dump());
    let ops = Json::Arr(vec![
        Json::obj().with("set", Json::obj().with("a", 200.0).with("b", "0x40")),
        Json::obj().with("read", Json::Arr(vec!["s".into()])),
    ]);
    assert_eq!(s.at(&["state"]).as_bool(), Some(false));
    let r = call("tb_run", Json::obj().with("id", s.at(&["id"]).clone()).with("ops", ops));
    assert_eq!(r.at(&["reads"]).dump(), r#"[{"cycle":0,"values":{"s":264}}]"#);

    let rom = "
module rom(in clk: clock, in addr: uint<2>, out q: uint<8>) {
    mem m: [uint<8>; 4];
    reg r: uint<8> init 0;
    r := m[addr];
    q = r;
}
";
    let s = call("tb_start", Json::obj().with("files", files(rom)));
    assert_eq!(s.at(&["ok"]).as_bool(), Some(true), "{}", s.dump());
    let mem = s.at(&["memories"]).as_array()[0].at(&["name"]).as_str().unwrap().to_string();
    let ops = Json::Arr(vec![
        Json::obj().with(
            "load",
            Json::obj()
                .with("mem", mem.as_str())
                .with("words", Json::Arr(vec![1.0.into(), 2.0.into(), 3.0.into(), 4.0.into()])),
        ),
        Json::obj().with("set", Json::obj().with("addr", 2.0)),
        Json::obj().with("step", 1.0),
        Json::obj().with("read", Json::Arr(vec!["q".into()])),
        Json::obj().with("dump", Json::obj().with("mem", mem.as_str()).with("count", 2.0)),
    ]);
    let r = call("tb_run", Json::obj().with("id", s.at(&["id"]).clone()).with("ops", ops));
    assert_eq!(r.at(&["ok"]).as_bool(), Some(true), "{}", r.dump());
    let reads = r.at(&["reads"]).as_array();
    assert_eq!(reads[0].at(&["values", "q"]).as_i64(), Some(3));
    assert_eq!(reads[1].at(&["words"]).dump(), "[1,2]");
}

#[test]
fn errors_come_back_with_diagnostics() {
    let s =
        call("tb_start", Json::obj().with("files", files("module m(out x: uint<2>) { x = 7; }")));
    assert_eq!(s.at(&["ok"]).as_bool(), Some(false));
    assert!(!s.at(&["diagnostics"]).as_array().is_empty(), "{}", s.dump());
}

#[test]
fn a_module_without_a_clock_port_gets_one() {
    let m = "module acc(in x: uint<4>, out total: uint<8>) {\n    reg t: uint<8> = 0;\n    t := t +% uint<8>(x);\n    total = t;\n}\n";
    let s = call("tb_start", Json::obj().with("files", files(m)));
    assert_eq!(s.at(&["ok"]).as_bool(), Some(true), "{}", s.dump());
    assert_eq!(s.at(&["clocks"]).dump(), r#"["clk"]"#);
    let clk = s.at(&["ports"]).as_array().iter().find(|p| p.at(&["name"]).as_str() == Some("clk"));
    assert_eq!(clk.unwrap().at(&["added"]).as_bool(), Some(true));
    assert_eq!(s.at(&["state"]).as_bool(), Some(true));
    let ops = Json::Arr(vec![
        Json::obj().with("set", Json::obj().with("x", 5.0)),
        Json::obj().with("step", 3.0),
        Json::obj().with("read", Json::Arr(vec!["total".into()])),
    ]);
    let r = call("tb_run", Json::obj().with("id", s.at(&["id"]).clone()).with("ops", ops));
    assert_eq!(r.at(&["reads"]).dump(), r#"[{"cycle":3,"values":{"total":15}}]"#);
}

#[test]
fn a_bitstream_decoded_back_to_cells_runs_and_a_flipped_bit_breaks_it() {
    let top = "module top(in clk: clock, in en: bool @clk, out q: uint<4>) {\n    reg c: uint<4> = 0;\n    if en { c := c +% 1; }\n    q = c;\n}\n";
    let pins = "set_io clk 15\nset_io en 1\nset_io q[0] 56\nset_io q[1] 57\nset_io q[2] 59\nset_io q[3] 60\n";
    let req = Json::obj()
        .with("files", files(top))
        .with("part", "hx1k")
        .with("package", "vq100")
        .with("pins", pins)
        .with("effort", 0.3);
    let out = call("run", req);
    assert_eq!(out.at(&["ok"]).as_bool(), Some(true), "{}", out.at(&["error"]).dump());
    let bin = out.at(&["bin"]).as_str().unwrap().to_string();
    let start = |flips: Json| {
        call(
            "tb_start",
            Json::obj()
                .with("bitstream", bin.as_str())
                .with("part", "hx1k")
                .with("package", "vq100")
                .with("pins", pins)
                .with("flips", flips),
        )
    };
    let count = |s: &Json| {
        let ops = Json::Arr(vec![
            Json::obj().with("set", Json::obj().with("en", 1.0)),
            Json::obj().with("step", 5.0).with("trace", Json::Arr(vec!["q".into()])),
        ]);
        let r = call("tb_run", Json::obj().with("id", s.at(&["id"]).clone()).with("ops", ops));
        assert_eq!(r.at(&["ok"]).as_bool(), Some(true), "{}", r.dump());
        r.at(&["reads"])
            .as_array()
            .iter()
            .map(|x| x.at(&["values", "q"]).as_i64().unwrap())
            .collect::<Vec<_>>()
    };
    let s = start(Json::Arr(vec![]));
    assert_eq!(s.at(&["ok"]).as_bool(), Some(true), "{}", s.dump());
    assert_eq!(count(&s), [1, 2, 3, 4, 5]);
    // Flip every set bit of the logic cells, one design at a time, until one
    // of them changes what the counter does.
    let mut broke = false;
    'tiles: for t in out.at(&["bitstream", "tiles"]).as_array() {
        for f in t.at(&["f"]).as_array() {
            if !f.at(&["n"]).as_str().unwrap_or("").starts_with("logic cell") {
                continue;
            }
            for b in f.at(&["b"]).as_array() {
                let flip = Json::Arr(vec![Json::Arr(vec![
                    t.at(&["x"]).clone(),
                    t.at(&["y"]).clone(),
                    b.clone(),
                ])]);
                let s = start(flip);
                assert_eq!(s.at(&["ok"]).as_bool(), Some(true), "{}", s.dump());
                if count(&s) != [1, 2, 3, 4, 5] {
                    broke = true;
                    break 'tiles;
                }
            }
        }
    }
    assert!(broke, "no flipped LUT bit changed the counter");
    let bad = start(Json::Arr(vec![Json::Arr(vec![0.0.into(), 0.0.into(), 0.0.into()])]));
    assert_eq!(bad.at(&["ok"]).as_bool(), Some(false));
}

#[test]
fn a_logic_cell_reports_where_its_truth_table_lives() {
    // Pins of the user's choosing, and pins of the placer's (whose router
    // moves the LUT's inputs to other pins).
    truth_table_bits("set_io a 1\nset_io b 2\nset_io y 56\n");
    truth_table_bits("");
}

fn truth_table_bits(pins: &str) {
    // An XOR in one logic cell: inverting every bit of its truth table, by
    // flipping the tile bits the cell reports, makes it an XNOR.
    let top = "module top(in a: bool, in b: bool, out y: bool) {\n    y = a ^ b;\n}\n";
    let req = Json::obj()
        .with("files", files(top))
        .with("part", "hx1k")
        .with("package", "vq100")
        .with("pins", pins);
    let out = call("run", req);
    assert_eq!(out.at(&["ok"]).as_bool(), Some(true), "{}", out.at(&["error"]).dump());
    let lc = out
        .at(&["chip", "cells"])
        .as_array()
        .iter()
        .find(|c| c.at(&["bel"]).as_str() == Some("lc"))
        .expect("a logic cell")
        .clone();
    let bits: Vec<i64> =
        lc.at(&["lut_bits"]).as_array().iter().map(|b| b.as_i64().unwrap()).collect();
    assert_eq!(bits.len(), 16);
    let (x, y) = (lc.at(&["x"]).clone(), lc.at(&["y"]).clone());
    let tile = out
        .at(&["bitstream", "tiles"])
        .as_array()
        .iter()
        .find(|t| t.at(&["x"]) == &x && t.at(&["y"]) == &y)
        .expect("the cell's tile")
        .clone();
    let set: Vec<i64> = tile.at(&["set"]).as_array().iter().map(|b| b.as_i64().unwrap()).collect();
    // The truth table's set bits are exactly the cell's LUT bits that are set.
    let lut: u32 =
        bits.iter().enumerate().filter(|(_, b)| set.contains(b)).map(|(i, _)| 1 << i).sum();
    assert_eq!(lut.count_ones(), 8, "an XOR of two of the four inputs: {lut:04x}");
    // The pins as placed, to name the decoded design's ports.
    let placed: String = out
        .at(&["chip", "cells"])
        .as_array()
        .iter()
        .filter_map(|c| {
            let name = c.at(&["name"]).as_str()?.strip_suffix("$sb_io")?;
            Some(format!("set_io {name} {}\n", c.at(&["pin"]).as_str()?))
        })
        .collect();
    let truth = |flips: Json| {
        let s = call(
            "tb_start",
            Json::obj()
                .with("bitstream", out.at(&["bin"]).clone())
                .with("part", "hx1k")
                .with("package", "vq100")
                .with("pins", placed.as_str())
                .with("flips", flips),
        );
        assert_eq!(s.at(&["ok"]).as_bool(), Some(true), "{}", s.dump());
        let mut got = vec![];
        for (a, b) in [(0.0, 0.0), (0.0, 1.0), (1.0, 0.0), (1.0, 1.0)] {
            let ops = Json::Arr(vec![
                Json::obj().with("set", Json::obj().with("a", a).with("b", b)),
                Json::obj().with("read", Json::Arr(vec!["y".into()])),
            ]);
            let r = call("tb_run", Json::obj().with("id", s.at(&["id"]).clone()).with("ops", ops));
            assert_eq!(r.at(&["ok"]).as_bool(), Some(true), "{}", r.dump());
            got.push(r.at(&["reads"]).as_array()[0].at(&["values", "y"]).as_i64().unwrap());
        }
        got
    };
    assert_eq!(truth(Json::Arr(vec![])), [0, 1, 1, 0]);
    let all = Json::Arr(
        bits.iter().map(|&b| Json::Arr(vec![x.clone(), y.clone(), (b as f64).into()])).collect(),
    );
    assert_eq!(truth(all), [1, 0, 0, 1]);
    // An empty table is a constant 0, and the cell, still routed, is kept.
    let clear = Json::Arr(
        bits.iter()
            .filter(|b| set.contains(b))
            .map(|&b| Json::Arr(vec![x.clone(), y.clone(), (b as f64).into()]))
            .collect(),
    );
    assert_eq!(truth(clear), [0, 0, 0, 0]);
    // The physical input that carries `a`: a table that copies it makes y = a.
    let nets = out.at(&["chip", "nets"]).as_array();
    let cells = out.at(&["chip", "cells"]).as_array();
    let from_a = |name: &Json| {
        nets.iter().any(|n| {
            n.at(&["name"]) == name
                && n.at(&["driver"]).as_i64().and_then(|d| cells[d as usize].at(&["name"]).as_str())
                    == Some("a$sb_io")
        })
    };
    let k = lc.at(&["lut_inputs"]).as_array().iter().position(from_a).expect("a reaches the LUT");
    let copy = Json::Arr(
        (0..16)
            .filter(|&i| ((i >> k) & 1 == 1) != ((lut >> i) & 1 == 1))
            .map(|i| Json::Arr(vec![x.clone(), y.clone(), (bits[i] as f64).into()]))
            .collect(),
    );
    assert_eq!(truth(copy), [0, 0, 1, 1]);
}

#[test]
fn a_run_can_trace_its_placement_and_routing() {
    let top = "module top(in clk: clock, in en: bool @clk, out q: uint<8>) {\n    reg c: uint<8> = 0;\n    if en { c := c +% 1; }\n    q = c;\n}\n";
    let req = Json::obj()
        .with("files", files(top))
        .with("part", "hx1k")
        .with("package", "vq100")
        .with("trace", true)
        .with("effort", 0.5);
    let out = call("run", req);
    assert_eq!(out.at(&["ok"]).as_bool(), Some(true), "{}", out.at(&["error"]).dump());
    let place = out.at(&["trace", "place"]).as_array();
    assert!(place.len() > 5, "{} placement steps", place.len());
    // The temperature falls, and the last snapshot is (nearly) the final placement.
    let t = |i: usize| place[i].at(&["temperature"]).as_f64().unwrap();
    assert!(t(place.len() - 1) < t(0));
    let last = place.last().unwrap().at(&["xy"]).as_array();
    let cells = out.at(&["chip", "cells"]).as_array();
    assert_eq!(last.len(), cells.len());
    // Logic cells are annealed; pads and global buffers are placed apart.
    let lcs: Vec<_> =
        cells.iter().zip(last).filter(|(c, _)| c.at(&["bel"]).as_str() == Some("lc")).collect();
    let same = lcs
        .iter()
        .filter(|(c, xy)| {
            xy.as_array()[0].as_i64() == c.at(&["x"]).as_i64()
                && xy.as_array()[1].as_i64() == c.at(&["y"]).as_i64()
        })
        .count();
    assert!(
        same * 10 >= lcs.len() * 7,
        "{same} of {} logic cells where the trace left them",
        lcs.len()
    );
    let route = out.at(&["trace", "route"]).as_array();
    assert!(!route.is_empty());
    assert_eq!(route.last().unwrap().at(&["overused"]).as_i64(), Some(0));
    // Without asking, there is no trace.
    let plain = call("run", Json::obj().with("files", files(top)).with("effort", 0.3));
    assert!(matches!(plain.at(&["trace"]), Json::Null));
}
