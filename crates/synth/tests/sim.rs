//! `synth sim`: Euler solvers reach their answers after their known cycle
//! counts, Weft designs print over their serial line, and the outputs (VCD,
//! C models) are well formed.
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sim(dir: &Path, args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_synth"))
        .current_dir(dir)
        .arg("sim")
        .args(args)
        .output()
        .unwrap();
    let (o, e) = (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap());
    assert!(out.status.success(), "synth sim {args:?} failed:\n{e}");
    (o, e)
}

#[test]
fn euler_solvers_answer_in_their_cycle_counts() {
    let d = root().join("tests/designs/euler");
    // (problem, extra source, answer, cycles), the short ones.
    for (n, extra, answer, cycles) in [
        ("025", Some("bcd.v"), 4782u64, 2390u64),
        ("031", None, 73682, 209),
        ("043", None, 16695334890, 363),
        ("092", None, 8581146, 1201),
        ("097", None, 8739992577, 389),
    ] {
        let top = format!("euler{}", n.trim_start_matches('0'));
        let file = format!("p{n}.v");
        let mut args = vec![file.as_str()];
        args.extend(extra);
        args.extend([
            "--top", &top, "--set", "start=1", "--set", "rst=1", "--at", "2:rst=0", "--until",
            "done", "--print", "answer",
        ]);
        for mode in ["--four-state", "--two-state"] {
            let mut a = args.clone();
            a.push(mode);
            let (out, err) = sim(&d, &a);
            assert!(out.contains(&format!("answer = {answer} ")), "p{n} {mode}: {out}");
            // Two cycles of reset, then the solver's own.
            assert!(err.contains(&format!("info: {} cycles", cycles + 2)), "p{n} {mode}: {err}");
        }
    }
}

#[test]
fn weft_hello_prints_over_its_serial_line() {
    let d = root().join("examples/weft");
    let (out, _) = sim(
        &d,
        &[
            "hello.weft",
            "uart.weft",
            "-P",
            "PERIOD=100",
            "-P",
            "CLKS_PER_BIT=4",
            "--cycles",
            "1500",
            "--uart",
            "tx:4",
        ],
    );
    assert!(out.starts_with("Hello, Weft!"), "{out:?}");
}

#[test]
fn vcd_and_c_model() {
    let dir = std::env::temp_dir().join(format!("synth-sim-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("count.v"),
        "module count(input clk, input rst, output reg [7:0] n);\n\
         always @(posedge clk) if (rst) n <= 0; else n <= n + 1;\nendmodule\n",
    )
    .unwrap();
    let (out, _) = sim(
        &dir,
        &[
            "count.v", "--set", "rst=1", "--at", "1:rst=0", "--cycles", "10", "--print", "n",
            "--vcd", "c.vcd",
        ],
    );
    assert!(out.contains("n = 9 "), "{out}");
    let vcd = std::fs::read_to_string(dir.join("c.vcd")).unwrap();
    assert!(vcd.contains("$var wire 8 ") && vcd.contains("$enddefinitions $end"), "{vcd}");
    assert!(vcd.contains("b00001001 "), "the counter reaches 9:\n{vcd}");
    sim(&dir, &["count.v", "--emit-c", "model"]);
    let h = std::fs::read_to_string(dir.join("model/count.h")).unwrap();
    assert!(h.contains("count_set_rst") && h.contains("count_get_n"), "{h}");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    if Command::new(&cc).arg("--version").output().is_ok() {
        std::fs::write(
            dir.join("model/main.c"),
            "#include <stdio.h>\n#include \"count.h\"\nint main(void) { static count_t m; int i;\n\
             count_init(&m); count_set_rst(&m, 1); count_set_clk(&m, 0); count_eval(&m);\n\
             count_set_clk(&m, 1); count_eval(&m); count_set_clk(&m, 0); count_eval(&m); count_set_rst(&m, 0);\n\
             for (i = 0; i < 300; i++) { count_set_clk(&m, 1); count_eval(&m); count_set_clk(&m, 0); count_eval(&m); }\n\
             printf(\"%u\\n\", (unsigned)count_get_n(&m)); return 0; }\n",
        )
        .unwrap();
        let st = Command::new(&cc)
            .current_dir(dir.join("model"))
            .args(["-O1", "-o", "count", "main.c", "count.c"])
            .status()
            .unwrap();
        assert!(st.success());
        let run = Command::new(dir.join("model/count")).output().unwrap();
        assert_eq!(String::from_utf8(run.stdout).unwrap().trim(), (300 % 256).to_string());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn synthesized_netlists_match_their_rtl() {
    let dir = std::env::temp_dir().join(format!("synth-sim-net-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Block RAMs in every width mode, initialized; the 16-bit one with byte
    // write enables (the bit mask).
    for (w, depth) in [(2, 2048), (4, 1024), (8, 512), (16, 256)] {
        let abits = (depth as u32).trailing_zeros();
        let write = if w == 16 {
            "if (we[0]) mem[wa][7:0] <= wd[7:0]; if (we[1]) mem[wa][15:8] <= wd[15:8];"
        } else {
            "if (we[0]) mem[wa] <= wd;"
        };
        let v = format!(
            "module ram(input clk, input [1:0] we, input [{a}:0] wa, input [{a}:0] ra, input [{d}:0] wd, output reg [{d}:0] q);\n\
             reg [{d}:0] mem [0:{last}];\n\
             integer i; initial for (i = 0; i < {depth}; i = i + 1) mem[i] = i * 7;\n\
             always @(posedge clk) begin q <= mem[ra]; {write} end\nendmodule\n",
            a = abits - 1,
            d = w - 1,
            last = depth - 1
        );
        std::fs::write(dir.join("ram.v"), v).unwrap();
        let (out, _) =
            sim(&dir, &["ram.v", "--compare", "synth", "--cycles", "3000", "--hold", "2"]);
        assert!(out.contains("PASS"), "{w}-bit RAM: {out}");
    }
    let _ = std::fs::remove_dir_all(&dir);
    // A CPU running firmware from block RAM.
    let soc = root().join("tests/designs/picosoc");
    let (out, _) = sim(
        &soc,
        &[
            "soc.v",
            "picorv32.v",
            "--top",
            "top",
            "--compare",
            "synth",
            "--cycles",
            "20000",
            "--hold",
            "5000",
        ],
    );
    assert!(out.contains("PASS"), "{out}");
    let euler = root().join("tests/designs/euler");
    let (out, err) = sim(
        &euler,
        &[
            "p025.v", "bcd.v", "--top", "euler25", "--set", "start=1", "--set", "rst=1", "--at",
            "2:rst=0", "--until", "done", "--print", "answer", "--level", "synth",
        ],
    );
    assert!(out.contains("answer = 4782 ") && err.contains("info: 2392 cycles"), "{out}{err}");
}

#[test]
fn bitstreams_match_their_rtl() {
    let d = root().join("tests/designs/euler");
    let (out, _) = sim(
        &d,
        &[
            "euler_top.v",
            "report.v",
            "p031.v",
            "-D",
            "SOLVER=euler31",
            "-D",
            "ANSWER_WIDTH=17",
            "--top",
            "euler_top",
            "--compare",
            "bitstream",
            "--device",
            "hx8k",
            "--package",
            "cb132",
            "--pcf",
            "alchitry_cu.pcf",
            "--clock",
            "clk",
            "--reset-low",
            "rst_n",
            "--cycles",
            "2000",
        ],
    );
    assert!(out.contains("PASS"), "{out}");
    // The answer's low byte on the LEDs, from the decoded bitstream.
    let (out, _) = sim(
        &d,
        &[
            "euler_top.v",
            "report.v",
            "p031.v",
            "-D",
            "SOLVER=euler31",
            "-D",
            "ANSWER_WIDTH=17",
            "--top",
            "euler_top",
            "--level",
            "bitstream",
            "--device",
            "hx8k",
            "--package",
            "cb132",
            "--pcf",
            "alchitry_cu.pcf",
            "--set",
            "rst_n=1",
            "--cycles",
            "400",
            "--print",
            "led",
        ],
    );
    assert!(out.contains("led = 210 "), "{out}");
}
