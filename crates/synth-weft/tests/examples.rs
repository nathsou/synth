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

#[test]
fn sources_are_formatted() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = vec![
        root.join("crates/synth-weft/src/std.weft"),
        root.join("crates/arch-ice40/src/prims.weft"),
    ];
    for e in std::fs::read_dir(root.join("examples/weft")).unwrap().flatten() {
        if e.path().extension().is_some_and(|x| x == "weft") {
            files.push(e.path());
        }
    }
    for f in files {
        let src = std::fs::read_to_string(&f).unwrap();
        assert_eq!(
            synth_weft::fmt::format(&src),
            src,
            "{} is not formatted (synth fmt)",
            f.display()
        );
    }
}

/// What rv_program.weft prints: a few results, the Fibonacci numbers up to
/// 610, and their sum.
fn riscv_expected() -> String {
    let mut out = String::new();
    for w in [
        0xffffff9cu32,
        0xffffffe7,
        0xffffff8b,
        0x0000f00d,
        0xf00d000d,
        0xfffff00d,
        0x000000f0,
        0x00000e20,
    ] {
        out += &format!("{w:08x}\r\n");
    }
    let (mut a, mut b) = (0u32, 1u32);
    for _ in 0..16 {
        out += &format!("{a:08x}\r\n");
        (a, b) = (b, a + b);
    }
    out + "0000063c\r\n"
}

/// Runs a RISC-V example until it has printed the program's output (or for
/// `max` cycles), and gives the cycles taken with the text.
fn run_riscv(core: &str, max: usize) -> (usize, String) {
    let m = compile_files(
        &[
            core,
            "examples/weft/rv32i.weft",
            "examples/weft/rv_program.weft",
            "examples/weft/uart.weft",
        ],
        &[],
        None,
        &[("CLKS_PER_BIT", "4"), ("DELAY", "0")],
    );
    let want = riscv_expected();
    let mut s = Sim::new(m);
    let mut line = Vec::new();
    for cycle in 0..max {
        line.push(s.peek("tx").unwrap());
        s.step();
        if cycle % 2048 == 0 || cycle == max - 1 {
            let text = String::from_utf8_lossy(&uart_decode(&line, 4)).into_owned();
            if text.len() >= want.len() {
                return (cycle, text[..want.len()].to_string());
            }
            if cycle == max - 1 {
                return (cycle, text);
            }
        }
    }
    unreachable!()
}

#[test]
fn riscv_single_cycle_runs_the_program() {
    let (_, text) = run_riscv("examples/weft/rv_single.weft", 60_000);
    assert_eq!(text, riscv_expected());
}

#[test]
fn riscv_multi_cycle_runs_the_program() {
    let (_, text) = run_riscv("examples/weft/rv_multi.weft", 150_000);
    assert_eq!(text, riscv_expected());
}

#[test]
fn riscv_pipelined_runs_the_program() {
    let (_, text) = run_riscv("examples/weft/rv_pipe.weft", 80_000);
    assert_eq!(text, riscv_expected());
}

#[test]
fn breathe_brightens_then_dims() {
    let m = compile_files(&["examples/weft/breathe.weft"], &[], None, &[("STEP", "256")]);
    let mut s = Sim::new(m);
    // The time LED 0 is on in each PWM period (256 cycles = one step).
    let mut duty = Vec::new();
    for _ in 0..72 {
        let mut on = 0;
        for _ in 0..256 {
            on += s.peek("led").unwrap() & 1;
            s.step();
        }
        duty.push(on);
    }
    assert!(duty[0] <= 1 && duty[63] >= 250, "{duty:?}");
    assert!(duty[..64].windows(2).all(|w| w[0] <= w[1] + 1), "{duty:?}");
    assert!(duty[64..].windows(2).all(|w| w[0] + 1 >= w[1]) && duty[71] < duty[64], "{duty:?}");
}

#[test]
fn life_draws_its_generations() {
    let m = compile_files(
        &["examples/weft/life.weft", "examples/weft/uart.weft"],
        &[],
        Some("top"),
        &[("CLKS_PER_BIT", "2"), ("PAUSE", "10")],
    );
    // The same rules, on the same torus.
    let mut g = [[false; 8]; 8];
    for (y, x) in [(0, 1), (1, 2), (2, 0), (2, 1), (2, 2), (5, 4), (5, 5), (5, 6)] {
        g[y][x] = true;
    }
    let mut frames = Vec::new();
    for _ in 0..3 {
        let mut f = String::from("\x1b[H");
        for row in &g {
            f.extend(row.iter().map(|&c| if c { '#' } else { '.' }));
            f += "\r\n";
        }
        frames.push(f);
        let mut n = [[false; 8]; 8];
        for y in 0..8 {
            for x in 0..8 {
                let mut count = 0;
                for (dy, dx) in [(7, 7), (7, 0), (7, 1), (0, 7), (0, 1), (1, 7), (1, 0), (1, 1)] {
                    count += g[(y + dy) % 8][(x + dx) % 8] as u32;
                }
                n[y][x] = count == 3 || (g[y][x] && count == 2);
            }
        }
        g = n;
    }
    let want = frames.concat();
    let mut s = Sim::new(m);
    let mut line = Vec::new();
    let mut text = String::new();
    for cycle in 1..=20_000 {
        line.push(s.peek("tx").unwrap());
        s.step();
        if cycle % 500 == 0 {
            text = String::from_utf8(uart_decode(&line, 2)).unwrap();
            if text.len() >= want.len() {
                break;
            }
        }
    }
    assert!(text.len() >= want.len(), "{text:?}");
    assert_eq!(&text[..want.len()], want);
}
