//! The designs in `examples/weft`, simulated.

mod common;

use common::{Sim, compile_files, uart_decode, verilog};

#[test]
fn blinky() {
    let m = compile_files(&["examples/weft/blinky.weft"], &[], None, &[]);
    let mut s = Sim::new(m);
    assert_eq!(s.peek("led"), Some(0));
    s.step();
    assert_eq!(s.peek("led"), Some(0));
}

#[test]
fn hello_prints_its_message() {
    let m = compile_files(
        &["examples/weft/hello.weft", "examples/weft/uart.weft"],
        &[],
        None,
        &[("PERIOD", "2000"), ("CLKS_PER_BIT", "8")],
    );
    let mut s = Sim::new(m);
    let mut line = Vec::new();
    for _ in 0..3500 {
        line.push(s.peek("tx").unwrap());
        s.step();
    }
    let text = String::from_utf8(uart_decode(&line, 8)).unwrap();
    assert_eq!(text, "Hello, Weft!\r\n");
}

#[test]
fn echo_returns_what_it_receives() {
    let m = compile_files(
        &["examples/weft/echo.weft", "examples/weft/uart.weft"],
        &[],
        None,
        &[("CLKS_PER_BIT", "8")],
    );
    let mut s = Sim::new(m);
    s.poke("sw1", 0);
    s.poke("rx", 1);
    // Serialize the input.
    let mut input = Vec::new();
    for &b in b"Weft" {
        input.extend(std::iter::repeat_n(1, 8 * 3));
        input.extend(std::iter::repeat_n(0, 8));
        for k in 0..8 {
            input.extend(std::iter::repeat_n(((b >> k) & 1) as u64, 8));
        }
        input.extend(std::iter::repeat_n(1, 8));
    }
    input.extend(std::iter::repeat_n(1, 8 * 60));
    let mut line = Vec::new();
    for v in input {
        s.poke("rx", v);
        s.step();
        line.push(s.peek("tx").unwrap());
    }
    assert_eq!(uart_decode(&line, 8), b"Weft");
}

#[test]
fn blinky_ram_matches_its_verilog_original() {
    let w = compile_files(&["examples/weft/blinky_ram.weft"], &[], None, &[]);
    let v = verilog(&["tests/designs/blinky_ram/blinky_ram.v"], "top");
    let (mut a, mut b) = (Sim::new(w), Sim::new(v));
    let mut rng = synth_core::Rng::new(7);
    for cycle in 0..20_000 {
        let btn = rng.next_u64().is_multiple_of(8) as u64;
        a.poke("btn", btn);
        b.poke("btn", btn);
        // The Verilog `q` has no initial value: compare once it is loaded.
        if cycle > 0 {
            assert_eq!(a.peek("led"), b.peek("led"), "led, cycle {cycle}");
        }
        assert_eq!(a.peek("tx"), b.peek("tx"), "tx, cycle {cycle}");
        a.step();
        b.step();
    }
}

#[test]
fn serv_port_matches_its_verilog_original() {
    let s = "tests/designs/third_party/serv";
    let sv: Vec<String> = [
        "servant/servant.v",
        "servant/servant_mux.v",
        "servant/servant_ram.v",
        "servant/servant_timer.v",
        "servant/servant_gpio.v",
    ]
    .iter()
    .map(|f| format!("{s}/{f}"))
    .chain(
        std::fs::read_dir(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(s).join("servile"),
        )
        .unwrap()
        .map(|e| format!("{s}/servile/{}", e.unwrap().file_name().to_string_lossy())),
    )
    .chain(
        std::fs::read_dir(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(s).join("rtl"),
        )
        .unwrap()
        .map(|e| format!("{s}/rtl/{}", e.unwrap().file_name().to_string_lossy())),
    )
    .filter(|f| f.ends_with(".v"))
    .collect();
    let mut sv_sorted = sv.clone();
    sv_sorted.sort();
    let refs: Vec<&str> = sv_sorted.iter().map(|s| s.as_str()).collect();
    // The memory file path is relative to the working directory.
    std::env::set_current_dir(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/weft"),
    )
    .unwrap();
    let w = compile_files(&["examples/weft/serv.weft"], &refs, Some("top"), &[]);
    let mut vfiles = vec!["examples/go_board_serv/top.v"];
    vfiles.extend(&refs);
    std::env::set_current_dir(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/go_board_serv"),
    )
    .unwrap();
    let v = verilog(&vfiles, "top");
    let (mut a, mut b) = (Sim::new(w), Sim::new(v));
    for cycle in 0..3000 {
        assert_eq!(a.peek("tx"), b.peek("tx"), "tx, cycle {cycle}");
        assert_eq!(a.peek("led"), b.peek("led"), "led, cycle {cycle}");
        a.step();
        b.step();
    }
}
