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
    let ops = Json::Arr(vec![
        Json::obj().with("set", Json::obj().with("x", 5.0)),
        Json::obj().with("step", 3.0),
        Json::obj().with("read", Json::Arr(vec!["total".into()])),
    ]);
    let r = call("tb_run", Json::obj().with("id", s.at(&["id"]).clone()).with("ops", ops));
    assert_eq!(r.at(&["reads"]).dump(), r#"[{"cycle":3,"values":{"total":15}}]"#);
}
