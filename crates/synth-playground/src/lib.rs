//! The synth playground: the whole flow, from source to bitstream, as data
//! for the web app in `web/`.
//!
//! [`handle`] answers the app's requests (JSON in, JSON out). It runs in the
//! browser as WebAssembly (see `wasm`), or behind the small HTTP server of
//! `synth playground` (see [`server`]).
//!
//! - `run`: elaborates, synthesises, places, routes and generates the
//!   bitstream, and returns every stage: diagnostics, the design as a graph
//!   and as text after each pass, the packed netlist, the placed and routed
//!   die, timing and the decoded configuration bits.
//! - `analyze`: diagnostics and the name index of Weft sources, for the
//!   editor (hover, go to definition).
//! - `format`: a Weft source, formatted.
//! - `examples`, `devices`: what the app offers to start from.

pub mod chip;
pub mod graph;
#[cfg(not(target_arch = "wasm32"))]
pub mod server;
pub mod sim;
pub mod text;
#[cfg(target_arch = "wasm32")]
mod wasm;

use synth_core::progress;
use synth_core::time::Instant;
use synth_core::{Diagnostics, FileId, Json, Severity, SourceMap};

/// A ready-made design.
#[derive(Debug)]
pub struct Example {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub files: &'static [(&'static str, &'static str)],
    /// The device it starts on; its pins come from the board for it.
    pub part: &'static str,
    pub package: &'static str,
    /// The top module, where it cannot be inferred (empty otherwise).
    pub top: &'static str,
}

/// A board, and the pins of every port the examples use on it, in its
/// family's constraints format.
#[derive(Debug)]
pub struct Board {
    pub name: &'static str,
    pub part: &'static str,
    pub package: &'static str,
    /// The frequency of the clock on `clk`, given to designs whose top has a
    /// `CLK_HZ` parameter.
    pub clock_hz: u64,
    pub pins: &'static str,
}

/// The FPGA families the playground implements designs for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Ice40,
    Artix7,
}

impl Family {
    pub fn of(part: &str) -> Family {
        if part.starts_with("xc7") { Family::Artix7 } else { Family::Ice40 }
    }
    pub fn name(self) -> &'static str {
        match self {
            Family::Ice40 => "iCE40",
            Family::Artix7 => "Artix-7",
        }
    }
    /// The constraints file of a project for this family.
    pub fn pins_file(self) -> &'static str {
        match self {
            Family::Ice40 => "pins.pcf",
            Family::Artix7 => "pins.xdc",
        }
    }
}

/// The Artix-7 parts offered (those of the boards), with their packages.
/// The playground uses speed grade -1.
pub const XC7_PARTS: &[(&str, &[&str])] = &[("xc7a35t", &["ftg256"]), ("xc7a100t", &["csg324"])];

pub const BOARDS: &[Board] = &[
    Board {
        name: "Nandland Go Board",
        part: "hx1k",
        package: "vq100",
        clock_hz: 25_000_000,
        pins: "# Nandland Go Board (iCE40 HX1K VQ100, 25 MHz)\nset_io clk 15\nset_frequency clk 25\nset_io sw1 53\nset_io btn 53\nset_io led[0] 56\nset_io led[1] 57\nset_io led[2] 59\nset_io led[3] 60\n# The seven-segment display's segments, for 8-bit outputs.\nset_io led[4] 3\nset_io led[5] 4\nset_io led[6] 7\nset_io led[7] 8\nset_io rx 73\nset_io tx 74\n",
    },
    Board {
        name: "Lattice iCEstick",
        part: "hx1k",
        package: "tq144",
        clock_hz: 12_000_000,
        pins: "# Lattice iCEstick (iCE40 HX1K TQ144, 12 MHz)\nset_io clk 21\nset_frequency clk 12\nset_io led[0] 99\nset_io led[1] 98\nset_io led[2] 97\nset_io led[3] 96\nset_io led[4] 95\nset_io rx 9\nset_io tx 8\n",
    },
    Board {
        name: "Lattice iCE40-HX8K Breakout Board",
        part: "hx8k",
        package: "ct256",
        clock_hz: 12_000_000,
        pins: "# Lattice iCE40-HX8K Breakout Board (iCE40 HX8K CT256, 12 MHz)\nset_io clk J3\nset_frequency clk 12\nset_io led[0] B5\nset_io led[1] B4\nset_io led[2] A2\nset_io led[3] A1\nset_io led[4] C5\nset_io led[5] C4\nset_io led[6] B3\nset_io led[7] C3\nset_io rx B10\nset_io tx B12\n",
    },
    Board {
        name: "Alchitry Cu",
        part: "hx8k",
        package: "cb132",
        clock_hz: 100_000_000,
        pins: "# Alchitry Cu (iCE40 HX8K CB132, 100 MHz)\nset_io clk P7\nset_frequency clk 100\nset_io sw1 P8\nset_io btn P8\nset_io led[0] J11\nset_io led[1] K11\nset_io led[2] K12\nset_io led[3] K14\nset_io led[4] L12\nset_io led[5] L14\nset_io led[6] M12\nset_io led[7] N14\nset_io rx P14\nset_io tx M9\n",
    },
    Board {
        name: "Alchitry Au",
        part: "xc7a35t",
        package: "ftg256",
        clock_hz: 100_000_000,
        pins: include_str!("boards/alchitry_au.xdc"),
    },
    Board {
        name: "Digilent Nexys A7-100T",
        part: "xc7a100t",
        package: "csg324",
        clock_hz: 100_000_000,
        pins: include_str!("boards/nexys_a7.xdc"),
    },
];

/// The board of a device, if one is known.
pub fn board(part: &str, package: &str) -> Option<&'static Board> {
    BOARDS.iter().find(|b| b.part == part && b.package == package)
}

/// The pin constraints for a device: its board's, or none (the placer then
/// chooses the pins).
pub fn board_pins(part: &str, package: &str) -> String {
    match board(part, package) {
        Some(b) => b.pins.to_string(),
        None => match Family::of(part) {
            Family::Ice40 => format!(
                "# No board is known for the {} {package}: the placer chooses the pins.\n# Add `set_io <port> <pin>` lines to fix them.\n",
                part.to_uppercase()
            ),
            Family::Artix7 => format!(
                "# No board is known for the {} {package}: give every port a pin with\n# set_property -dict {{PACKAGE_PIN <pin> IOSTANDARD LVCMOS33}} [get_ports {{<port>}}]\n",
                part.to_uppercase()
            ),
        },
    }
}

const UART: &str = include_str!("../../../examples/weft/uart.weft");
const RV32I: &str = include_str!("../../../examples/weft/rv32i.weft");
const RV_PROGRAM: &str = include_str!("../../../examples/weft/rv_program.weft");

pub const EXAMPLES: &[Example] = &[
    Example {
        id: "blinky",
        title: "Blinky",
        description: "A counter on four LEDs",
        files: &[("blinky.weft", include_str!("../../../examples/weft/blinky.weft"))],
        part: "hx1k",
        package: "vq100",
        top: "",
    },
    Example {
        id: "hello",
        title: "Hello UART",
        description: "Prints a greeting over a UART once a second",
        files: &[
            ("hello.weft", include_str!("../../../examples/weft/hello.weft")),
            ("uart.weft", UART),
        ],
        part: "hx1k",
        package: "vq100",
        top: "",
    },
    Example {
        id: "echo",
        title: "UART echo",
        description: "Echoes a UART through a FIFO in block RAM",
        files: &[
            ("echo.weft", include_str!("../../../examples/weft/echo.weft")),
            ("uart.weft", UART),
        ],
        part: "hx1k",
        package: "vq100",
        top: "",
    },
    Example {
        id: "blinky_ram",
        title: "Blinky RAM",
        description: "LEDs driven through an initialized block RAM",
        files: &[("blinky_ram.weft", include_str!("../../../examples/weft/blinky_ram.weft"))],
        part: "hx1k",
        package: "vq100",
        top: "",
    },
    Example {
        id: "breathe",
        title: "Breathing LEDs",
        description: "Pulse-width modulation through a gamma table computed at compile time",
        files: &[("breathe.weft", include_str!("../../../examples/weft/breathe.weft"))],
        part: "hx1k",
        package: "vq100",
        top: "",
    },
    Example {
        id: "life",
        title: "Game of Life",
        description: "Conway's Game of Life on an 8×8 torus, drawn over a UART",
        files: &[
            ("life.weft", include_str!("../../../examples/weft/life.weft")),
            ("uart.weft", UART),
        ],
        part: "hx1k",
        package: "vq100",
        top: "",
    },
    Example {
        id: "rv_single",
        title: "RISC-V, single-cycle",
        description: "An RV32I core that runs each instruction in one cycle",
        files: &[
            ("rv_single.weft", include_str!("../../../examples/weft/rv_single.weft")),
            ("rv32i.weft", RV32I),
            ("rv_program.weft", RV_PROGRAM),
            ("uart.weft", UART),
        ],
        part: "hx8k",
        package: "ct256",
        top: "",
    },
    Example {
        id: "rv_multi",
        title: "RISC-V, multi-cycle",
        description: "An RV32I core that runs each instruction in five cycles, one per step",
        files: &[
            ("rv_multi.weft", include_str!("../../../examples/weft/rv_multi.weft")),
            ("rv32i.weft", RV32I),
            ("rv_program.weft", RV_PROGRAM),
            ("uart.weft", UART),
        ],
        part: "hx8k",
        package: "ct256",
        top: "",
    },
    Example {
        id: "rv_pipe",
        title: "RISC-V, pipelined",
        description: "An RV32I core in five pipeline stages, with forwarding",
        files: &[
            ("rv_pipe.weft", include_str!("../../../examples/weft/rv_pipe.weft")),
            ("rv32i.weft", RV32I),
            ("rv_program.weft", RV_PROGRAM),
            ("uart.weft", UART),
        ],
        part: "hx8k",
        package: "ct256",
        top: "",
    },
    Example {
        id: "blinky_ram_v",
        title: "Blinky RAM (Verilog)",
        description: "The Verilog original of Blinky RAM",
        files: &[("blinky_ram.v", include_str!("../../../tests/designs/blinky_ram/blinky_ram.v"))],
        part: "hx1k",
        package: "vq100",
        top: "",
    },
    Example {
        id: "euler10",
        title: "Euler 10: Summation of primes",
        description: "Project Euler 10 in hardware: sends 142913828922 over the UART after 131,016 cycles",
        files: &[
            ("top.v", euler_top!("euler10", 38, "40000000")),
            ("p010.v", include_str!("../../../tests/designs/euler/p010.v")),
            ("euler_board.v", EULER_BOARD),
            ("report.v", EULER_REPORT),
        ],
        part: "hx8k",
        package: "cb132",
        top: "top",
    },
    Example {
        id: "euler14",
        title: "Euler 14: Longest Collatz sequence",
        description: "Project Euler 14 in hardware: sends 837799 over the UART after 364,431 cycles",
        files: &[
            ("top.v", euler_top!("euler14", 20, "33333333")),
            ("p014.v", include_str!("../../../tests/designs/euler/p014.v")),
            ("euler_board.v", EULER_BOARD),
            ("report.v", EULER_REPORT),
        ],
        part: "hx8k",
        package: "cb132",
        top: "top",
    },
    Example {
        id: "euler25",
        title: "Euler 25: 1000-digit Fibonacci number",
        description: "Project Euler 25 in hardware: sends 4782 over the UART after 2,390 cycles",
        files: &[
            ("top.v", euler_top!("euler25", 16, "50000000")),
            ("p025.v", include_str!("../../../tests/designs/euler/p025.v")),
            ("bcd.v", EULER_BCD),
            ("euler_board.v", EULER_BOARD),
            ("report.v", EULER_REPORT),
        ],
        part: "hx8k",
        package: "cb132",
        top: "top",
    },
    Example {
        id: "euler31",
        title: "Euler 31: Coin sums",
        description: "Project Euler 31 in hardware: sends 73682 over the UART after 209 cycles",
        files: &[
            ("top.v", euler_top!("euler31", 17, "100000000")),
            ("p031.v", include_str!("../../../tests/designs/euler/p031.v")),
            ("euler_board.v", EULER_BOARD),
            ("report.v", EULER_REPORT),
        ],
        part: "hx8k",
        package: "cb132",
        top: "top",
    },
    Example {
        id: "euler43",
        title: "Euler 43: Sub-string divisibility",
        description: "Project Euler 43 in hardware: sends 16695334890 over the UART after 363 cycles",
        files: &[
            ("top.v", euler_top!("euler43", 36, "40000000")),
            ("p043.v", include_str!("../../../tests/designs/euler/p043.v")),
            ("euler_board.v", EULER_BOARD),
            ("report.v", EULER_REPORT),
        ],
        part: "hx8k",
        package: "cb132",
        top: "top",
    },
    Example {
        id: "euler48",
        title: "Euler 48: Self powers",
        description: "Project Euler 48 in hardware: sends 9110846700 over the UART after 22,758 cycles",
        files: &[
            ("top.v", euler_top!("euler48", 34, "33333333")),
            ("p048.v", include_str!("../../../tests/designs/euler/p048.v")),
            ("euler_board.v", EULER_BOARD),
            ("report.v", EULER_REPORT),
        ],
        part: "hx8k",
        package: "cb132",
        top: "top",
    },
    Example {
        id: "euler57",
        title: "Euler 57: Square root convergents",
        description: "Project Euler 57 in hardware: sends 153 over the UART after 8,571 cycles",
        files: &[
            ("top.v", euler_top!("euler57", 10, "50000000")),
            ("p057.v", include_str!("../../../tests/designs/euler/p057.v")),
            ("bcd.v", EULER_BCD),
            ("euler_board.v", EULER_BOARD),
            ("report.v", EULER_REPORT),
        ],
        part: "hx8k",
        package: "cb132",
        top: "top",
    },
    Example {
        id: "euler58",
        title: "Euler 58: Spiral primes",
        description: "Project Euler 58 in hardware: sends 26241 over the UART after 2,628,585 cycles",
        files: &[
            ("top.v", euler_top!("euler58", 16, "25000000")),
            ("p058.v", include_str!("../../../tests/designs/euler/p058.v")),
            ("euler_board.v", EULER_BOARD),
            ("report.v", EULER_REPORT),
        ],
        part: "hx8k",
        package: "cb132",
        top: "top",
    },
    Example {
        id: "euler92",
        title: "Euler 92: Square digit chains",
        description: "Project Euler 92 in hardware: sends 8581146 over the UART after 1,201 cycles",
        files: &[
            ("top.v", euler_top!("euler92", 24, "66666667")),
            ("p092.v", include_str!("../../../tests/designs/euler/p092.v")),
            ("euler_board.v", EULER_BOARD),
            ("report.v", EULER_REPORT),
        ],
        part: "hx8k",
        package: "cb132",
        top: "top",
    },
    Example {
        id: "euler97",
        title: "Euler 97: Large non-Mersenne prime",
        description: "Project Euler 97 in hardware: sends 8739992577 over the UART after 389 cycles",
        files: &[
            ("top.v", euler_top!("euler97", 34, "66666667")),
            ("p097.v", include_str!("../../../tests/designs/euler/p097.v")),
            ("euler_board.v", EULER_BOARD),
            ("report.v", EULER_REPORT),
        ],
        part: "hx8k",
        package: "cb132",
        top: "top",
    },
];

/// The playground's top for a Project Euler solver: the solver with the
/// board-neutral shell of `tests/designs/euler/euler_board.v`, on a clock of
/// at most `max_hz` (its speed on the Alchitry Cu) from the board's PLL.
macro_rules! euler_top {
    ($solver:literal, $width:literal, $max_hz:literal) => {
        concat!(
            "// Project Euler in hardware: ", $solver, " with a board-neutral shell (euler_board.v).\n",
            "// \"<answer> <cycles>\" on the UART at 115200 baud; the answer's low byte on the LEDs.\n",
            "// The solver runs at SYS_HZ, MAX_HZ at most (it meets timing at that speed on\n",
            "// every board), from the board's PLL.\n",
            "module top #(parameter CLK_HZ = 100000000, parameter MAX_HZ = ", $max_hz, ")\n",
            "  (input clk, output tx, output [7:0] led);\n",
            "  localparam SYS_HZ = MAX_HZ < CLK_HZ ? MAX_HZ : CLK_HZ;\n",
            "  wire sys, locked, rst, done;\n",
            "  wire [", $width, "-1:0] answer;\n",
            "  synth_pll #(.IN_HZ(CLK_HZ), .OUT_HZ(SYS_HZ)) pll (.clk_in(clk), .clk_out(sys),\n",
            "    .locked(locked));\n",
            "  ", $solver, " solver (.clk(sys), .rst(rst), .start(1'b1), .done(done), .answer(answer));\n",
            "  euler_board #(.CLK_HZ(SYS_HZ), .ANSWER_WIDTH(", $width, ")) board (.clk(sys),\n",
            "    .locked(locked), .rst(rst), .done(done), .answer(answer), .tx(tx), .led(led));\n",
            "endmodule\n",
        )
    };
}
use euler_top;

const EULER_BOARD: &str = include_str!("../../../tests/designs/euler/euler_board.v");
const EULER_REPORT: &str = include_str!("../../../tests/designs/euler/report.v");
const EULER_BCD: &str = include_str!("../../../tests/designs/euler/bcd.v");

/// Like [`handle`], with `progress` receiving where the flow is as it runs:
/// `{"step", "fraction", "detail"}`, `step` one of
/// [`synth_core::progress::STEPS`] and `fraction` (0 to 1) null when the
/// step cannot tell.
pub fn handle_observed(
    method: &str,
    body: &str,
    mut progress: impl FnMut(&Json) + 'static,
) -> String {
    progress::observe(
        move |p| {
            progress(
                &Json::obj()
                    .with("step", p.step)
                    .with("fraction", p.fraction.map_or(Json::Null, Json::from))
                    .with("detail", p.detail.as_str()),
            )
        },
        || handle(method, body),
    )
}

/// Answers one request of the web app. Unknown methods and malformed
/// requests give `{"error": ...}`.
pub fn handle(method: &str, body: &str) -> String {
    let req = if body.trim().is_empty() { Ok(Json::obj()) } else { Json::parse(body) };
    let req = match req {
        Ok(r) => r,
        Err(e) => return Json::obj().with("error", format!("invalid request: {e}")).dump(),
    };
    let out = match method {
        "run" => run(&req),
        "analyze" => analyze(&req),
        "format" => {
            let text = req.at(&["text"]).as_str().unwrap_or("");
            Json::obj().with("text", synth_weft::fmt::format(text))
        }
        "examples" => examples(),
        "devices" => devices(),
        "sim_start" => sim_start(&req),
        "sim_step" => sim::step(&req).unwrap_or_else(|e| Json::obj().with("error", e)),
        "sim_stop" => {
            sim::stop();
            Json::obj()
        }
        other => Json::obj().with("error", format!("unknown method `{other}`")),
    };
    out.dump()
}

pub fn examples() -> Json {
    Json::Arr(
        EXAMPLES
            .iter()
            .map(|e| {
                let files: Vec<Json> = e
                    .files
                    .iter()
                    .map(|(n, t)| Json::obj().with("name", *n).with("text", *t))
                    .collect();
                Json::obj()
                    .with("id", e.id)
                    .with("title", e.title)
                    .with("description", e.description)
                    .with("files", files)
                    .with("pins", board_pins(e.part, e.package))
                    .with("pins_file", Family::of(e.part).pins_file())
                    .with("part", e.part)
                    .with("package", e.package)
                    .with("top", e.top)
            })
            .collect(),
    )
}

/// The supported parts, their packages, and the pins, board and clock of
/// each.
pub fn devices() -> Json {
    use arch_ice40::arch::Part;
    let device = |part: &str, die: &str, packages: Vec<&str>| {
        let family = Family::of(part);
        let per = |f: &dyn Fn(&str) -> Option<Json>| {
            Json::Obj(packages.iter().filter_map(|k| Some((k.to_string(), f(k)?))).collect())
        };
        Json::obj()
            .with("part", part)
            .with("family", family.name())
            .with("die", die)
            .with("pins_file", family.pins_file())
            .with("pins", per(&|k| Some(board_pins(part, k).into())))
            .with("boards", per(&|k| board(part, k).map(|b| b.name.into())))
            .with("clock_hz", per(&|k| board(part, k).map(|b| (b.clock_hz as i64).into())))
            .with("packages", packages.clone())
    };
    let mut out: Vec<Json> = Part::ALL
        .iter()
        .map(|p| {
            let db = arch_ice40::chipdb::ChipDb::get(p.die());
            let names: Vec<&str> = db
                .packages
                .iter()
                .filter(|k| arch_ice40::arch::Arch::new(*p, &k.name).is_ok())
                .map(|k| k.name.as_str())
                .collect();
            device(p.name(), p.die().name(), names)
        })
        .collect();
    for (part, packages) in XC7_PARTS {
        out.push(device(part, part, packages.to_vec()));
    }
    Json::Arr(out)
}

fn is_weft(name: &str) -> bool {
    name.ends_with(".weft")
}

/// Source files of a request, added to a source map.
fn sources(req: &Json, sm: &mut SourceMap) -> Vec<(FileId, String)> {
    req.at(&["files"])
        .as_array()
        .iter()
        .filter_map(|f| {
            let name = f.at(&["name"]).as_str()?.to_string();
            let text = f.at(&["text"]).as_str().unwrap_or("");
            Some((sm.add(name.clone(), text), name))
        })
        .collect()
}

/// Diagnostics with positions, for the editor and the problems list.
pub fn diagnostics_json(sm: &SourceMap, diags: &Diagnostics) -> Json {
    let loc = |sp: synth_core::Span| {
        let f = sm.file(sp.file);
        let (l, c) = f.line_col(sp.start as usize);
        let (el, ec) = f.line_col(sp.end as usize);
        Json::obj()
            .with("file", f.name.as_str())
            .with("start", sp.start)
            .with("end", sp.end)
            .with("line", l)
            .with("col", c)
            .with("eline", el)
            .with("ecol", ec)
    };
    Json::Arr(
        diags
            .items
            .iter()
            .map(|d| {
                let sev = match d.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                    Severity::Note => "note",
                };
                let mut j = Json::obj().with("severity", sev).with("message", d.message.as_str());
                if let Some(sp) = d.span {
                    j.set("at", loc(sp));
                }
                if let Some(l) = &d.label {
                    j.set("label", l.as_str());
                }
                let labels: Vec<Json> =
                    d.labels.iter().map(|(sp, m)| loc(*sp).with("message", m.as_str())).collect();
                if !labels.is_empty() {
                    j.set("labels", labels);
                }
                let notes: Vec<Json> = d
                    .notes
                    .iter()
                    .map(|(sp, m)| {
                        let mut n = Json::obj().with("message", m.as_str());
                        if let Some(sp) = sp {
                            n.set("at", loc(*sp));
                        }
                        n
                    })
                    .collect();
                if !notes.is_empty() {
                    j.set("notes", notes);
                }
                if !d.helps.is_empty() {
                    j.set("helps", d.helps.clone());
                }
                j.set("rendered", d.render(sm));
                j
            })
            .collect(),
    )
}

const PRELUDE: (&str, &str) = ("ice40 primitives", arch_ice40::WEFT_PRELUDE);

/// Diagnostics and the name index of the request's sources.
pub fn analyze(req: &Json) -> Json {
    let mut sm = SourceMap::new();
    let files = sources(req, &mut sm);
    let weft: Vec<FileId> = files.iter().filter(|(_, n)| is_weft(n)).map(|(f, _)| *f).collect();
    let verilog: Vec<FileId> = files.iter().filter(|(_, n)| !is_weft(n)).map(|(f, _)| *f).collect();
    let mut diags = Diagnostics::default();
    let src = if verilog.is_empty() {
        None
    } else {
        let (src, d) = synth_verilog::parse_files(&mut sm, &verilog, &Default::default());
        let failed = d.has_errors();
        diags.extend(d);
        (!failed).then_some(src)
    };
    let mut refs = Vec::new();
    if !weft.is_empty() {
        let opts = options(req);
        let a = synth_weft::analyze(&mut sm, &weft, &[PRELUDE], src.as_ref(), &opts);
        diags.extend(a.diags);
        for r in a.refs {
            let name = |f: FileId| sm.file(f).name.clone();
            let mut j = Json::obj()
                .with("file", name(r.span.file))
                .with("start", r.span.start)
                .with("end", r.span.end)
                .with("kind", format!("{:?}", r.kind))
                .with("hover", r.hover);
            if let Some(d) = r.decl {
                j.set(
                    "decl",
                    Json::obj()
                        .with("file", name(d.file))
                        .with("start", d.start)
                        .with("end", d.end),
                );
            }
            if !r.members.is_empty() {
                j.set("members", r.members);
            }
            refs.push(j);
        }
    }
    Json::obj().with("diagnostics", diagnostics_json(&sm, &diags)).with("refs", refs)
}

fn options(req: &Json) -> synth_weft::Options {
    let params = match req.at(&["params"]) {
        Json::Obj(m) => m
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    Json::Str(s) => s.clone(),
                    other => other.dump(),
                };
                (k.clone(), v)
            })
            .collect(),
        _ => Vec::new(),
    };
    let top = req.at(&["top"]).as_str().filter(|t| !t.is_empty()).map(String::from);
    let defaults = clock_hz(req).map(|hz| vec![("CLK_HZ".to_string(), hz.to_string())]);
    synth_weft::Options {
        top,
        params,
        defaults: defaults.unwrap_or_default(),
        ..Default::default()
    }
}

/// The board's clock frequency, given to a top with a `CLK_HZ` parameter.
fn clock_hz(req: &Json) -> Option<u64> {
    req.at(&["clock_hz"]).as_i64().filter(|&hz| hz > 0).map(|hz| hz as u64)
}

/// The pin constraints of a request (`pcf` before Artix-7 support).
fn pins(req: &Json) -> &str {
    req.at(&["pins"]).as_str().or_else(|| req.at(&["pcf"]).as_str()).unwrap_or("")
}

/// Above this many AND nodes the gate-level graph is left out.
const MAX_AIG_GRAPH: usize = 40_000;

struct Run {
    log: Vec<String>,
    stages: Vec<Json>,
    times: Vec<(String, f64)>,
}

impl Run {
    fn time(&mut self, what: &str, t: Instant) {
        self.times.push((what.to_string(), t.elapsed().as_secs_f64() * 1000.0));
    }
}

fn stats_json(stats: Vec<(String, usize)>) -> Json {
    Json::Obj(stats.into_iter().map(|(k, n)| (k, Json::from(n))).collect())
}

/// The whole flow.
pub fn run(req: &Json) -> Json {
    let mut r = Run { log: Vec::new(), stages: Vec::new(), times: Vec::new() };
    let mut out = Json::obj();
    let result = run_inner(req, &mut r, &mut out);
    out.set("ok", result.is_ok());
    if let Err(e) = result {
        out.set("error", e);
    }
    out.set("log", r.log);
    out.set("stages", r.stages);
    out.set(
        "times",
        Json::Arr(
            r.times
                .into_iter()
                .map(|(a, b)| Json::Arr(vec![a.into(), ((b * 10.0).round() / 10.0).into()]))
                .collect(),
        ),
    );
    out
}

/// Elaborates the request's sources (Weft or Verilog), its board clock as
/// the top's `CLK_HZ`: the module, if there were no errors, the
/// diagnostics and the number of errors.
fn elaborate(req: &Json) -> Result<(Option<synth_rtl::Module>, Json, usize), String> {
    let mut sm = SourceMap::new();
    let files = sources(req, &mut sm);
    if files.is_empty() {
        return Err("no source files".into());
    }
    let weft: Vec<FileId> = files.iter().filter(|(_, n)| is_weft(n)).map(|(f, _)| *f).collect();
    let verilog: Vec<FileId> = files.iter().filter(|(_, n)| !is_weft(n)).map(|(f, _)| *f).collect();
    let mut diags = Diagnostics::default();
    let src = if verilog.is_empty() {
        None
    } else {
        let (src, d) = synth_verilog::parse_files(&mut sm, &verilog, &Default::default());
        diags.extend(d);
        Some(src)
    };
    let module = if diags.has_errors() {
        None
    } else if !weft.is_empty() {
        let (m, d) = synth_weft::compile(&mut sm, &weft, &[PRELUDE], src.as_ref(), &options(req));
        diags.extend(d);
        m
    } else {
        let top = req.at(&["top"]).as_str().filter(|t| !t.is_empty()).map(String::from);
        // Verilog ignores parameters the top does not declare.
        let top_params = clock_hz(req)
            .map(|hz| vec![("CLK_HZ".to_string(), synth_core::LogicVec::from_u64(hz, 32))])
            .unwrap_or_default();
        let opts = synth_elab::ElabOptions { top, top_params };
        let (m, d) =
            synth_elab::elaborate(src.as_ref().unwrap(), &sm, &opts, &synth_elab::StdBlackBoxes);
        diags.extend(d);
        m
    };
    Ok((module, diagnostics_json(&sm, &diags), diags.error_count()))
}

/// Starts a simulation of the design (see [`sim`]): its clock at
/// `sim::DEFAULT_HZ`, given to the design as `CLK_HZ`.
fn sim_start(req: &Json) -> Json {
    let mut req = req.clone();
    req.set("clock_hz", sim::DEFAULT_HZ);
    let (module, diagnostics, errors) = match elaborate(&req) {
        Ok(e) => e,
        Err(e) => return Json::obj().with("ok", false).with("error", e),
    };
    let Some(module) = module else {
        return Json::obj()
            .with("ok", false)
            .with("error", format!("{} error(s)", errors.max(1)))
            .with("diagnostics", diagnostics);
    };
    match sim::start(module, sim::DEFAULT_HZ) {
        Ok(info) => info.with("ok", true).with("diagnostics", diagnostics),
        Err(e) => Json::obj().with("ok", false).with("error", e).with("diagnostics", diagnostics),
    }
}

fn run_inner(req: &Json, r: &mut Run, out: &mut Json) -> Result<(), String> {
    // Elaboration.
    let t = Instant::now();
    progress::step("elaborate", "");
    let (module, diagnostics, errors) = elaborate(req)?;
    out.set("diagnostics", diagnostics);
    r.time("elaborate", t);
    let Some(module) = module else {
        return Err(format!("{} error(s)", errors.max(1)));
    };
    out.set("top", module.name.as_str());
    let part = req.at(&["part"]).as_str().unwrap_or("hx1k").to_string();
    if Family::of(&part) == Family::Artix7 {
        return run_xc7(module, &part, req, r, out);
    }

    // Synthesis, observed pass by pass.
    let t = Instant::now();
    let stages = &mut r.stages;
    progress::step("synthesize", "");
    let mut observe = |s: arch_ice40::synth::SynthStage| match s {
        arch_ice40::synth::SynthStage::Rtl(name, m) => {
            let id = name.replace(' ', "_");
            stages.push(
                Json::obj()
                    .with("id", id)
                    .with("title", format!("RTL, {name}"))
                    .with("kind", "rtl")
                    .with("stats", stats_json(m.stats()))
                    .with("graph", graph::rtl_graph(m))
                    .with("text", text::rtl_text(m)),
            );
        }
        arch_ice40::synth::SynthStage::Logic(name, l) => {
            let ands = l.aig.num_ands();
            let mut j = Json::obj()
                .with("id", format!("gates_{}", name.replace(' ', "_")))
                .with("title", format!("Gates, {name}"))
                .with("kind", "aig")
                .with(
                    "stats",
                    Json::obj()
                        .with("AND", ands)
                        .with("FF", l.ffs.len())
                        .with("carry chains", l.carries.len())
                        .with("instances", l.instances.len()),
                );
            if ands <= MAX_AIG_GRAPH {
                j.set("graph", graph::aig_graph(l));
            }
            j.set("text", text::aig_text(l));
            stages.push(j);
        }
        arch_ice40::synth::SynthStage::Mapped(nl) => {
            stages.push(
                Json::obj()
                    .with("id", "mapped")
                    .with("title", "Mapped to iCE40 cells")
                    .with("kind", "netlist")
                    .with("stats", stats_json(nl.stats()))
                    .with("graph", graph::netlist_graph(nl))
                    .with("text", text::netlist_text(nl)),
            );
        }
    };
    let (nl, log) =
        arch_ice40::synth::synthesize_observed(module, &Default::default(), &mut observe)?;
    r.log.extend(log);
    r.time("synthesise", t);
    if req.at(&["stop"]).as_str() == Some("synth") {
        return Ok(());
    }

    // Place and route.
    let t = Instant::now();
    let part: arch_ice40::arch::Part = part.parse()?;
    let package = req.at(&["package"]).as_str().unwrap_or("vq100").to_string();
    let pcf = arch_ice40::pcf::parse_pcf(pins(req))?;
    let mut clocks = pcf.clocks.clone();
    clocks.default_mhz = req.at(&["freq"]).as_f64().filter(|f| *f > 0.0);
    let opts = arch_ice40::flow::FlowOptions {
        part,
        package,
        seed: req.at(&["seed"]).as_i64().unwrap_or(1).max(0) as u64,
        place_effort: req.at(&["effort"]).as_f64().unwrap_or(1.0),
        timing_weight: req
            .at(&["timing_weight"])
            .as_f64()
            .unwrap_or(arch_ice40::flow::TIMING_WEIGHT)
            .clamp(0.0, 1.0),
        clocks,
        route_timing_rounds: arch_ice40::flow::ROUTE_TIMING_ROUNDS,
        tries: req.at(&["tries"]).as_i64().map_or(1, |n| n.max(1) as usize),
    };
    let (res, imp) = arch_ice40::flow::implement_detailed(nl, &pcf.pins, &opts).map_err(|e| {
        // The log of a failed run comes first, as `info:` lines.
        let mut msg = Vec::new();
        for l in e.lines() {
            match l.strip_prefix("info: ") {
                Some(i) => r.log.push(i.to_string()),
                None => msg.push(l),
            }
        }
        msg.join("\n")
    })?;
    r.log.extend(res.log.iter().cloned());
    r.time("place and route", t);
    progress::note(|| "preparing the views".into());
    r.stages.push(
        Json::obj()
            .with("id", "packed")
            .with("title", "Packed into logic cells")
            .with("kind", "netlist")
            .with("stats", stats_json(imp.netlist.stats()))
            .with("graph", graph::netlist_graph(&imp.netlist))
            .with("text", text::netlist_text(&imp.netlist)),
    );
    let t = Instant::now();
    let (chip, timing) = chip::chip(&imp, &opts);
    out.set("chip", chip);
    out.set("timing", timing.with("met", res.timing_met));
    out.set("bitstream", chip::bitstream(&imp.arch, &res.config));
    out.set("bin", base64(&res.config.to_bin()));
    out.set("bin_name", format!("{}.bin", out.at(&["top"]).as_str().unwrap_or("design")));
    out.set("family", Family::Ice40.name());
    r.time("export", t);
    Ok(())
}

/// Where the Project X-Ray database is: set by `synth playground --xc7-db`
/// or `SYNTH_XC7_DB`.
pub static XC7_DB: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

fn xc7_root() -> Result<std::path::PathBuf, String> {
    // On WebAssembly, the worker serves the database's files from the
    // part's bundle (see `tools/build_playground.sh`).
    if cfg!(target_arch = "wasm32") {
        return Ok("xc7db".into());
    }
    if let Some(p) = XC7_DB.get() {
        return Ok(p.clone());
    }
    match std::env::var_os("SYNTH_XC7_DB") {
        Some(p) => Ok(p.into()),
        None => Err("Artix-7 needs the Project X-Ray database: run `tools/fetch_xc7_db.sh <dir>`, then start `synth playground --xc7-db <dir>/artix7` (or set SYNTH_XC7_DB)".into()),
    }
}

/// The Artix-7 flow: synthesis, then packing, placement, routing, timing and
/// the bitstream in one step (it does not expose its intermediate states).
fn run_xc7(
    module: synth_rtl::Module,
    part: &str,
    req: &Json,
    r: &mut Run,
    out: &mut Json,
) -> Result<(), String> {
    let t = Instant::now();
    r.stages.push(
        Json::obj()
            .with("id", "elaborated")
            .with("title", "RTL, elaborated")
            .with("kind", "rtl")
            .with("stats", stats_json(module.stats()))
            .with("graph", graph::rtl_graph(&module))
            .with("text", text::rtl_text(&module)),
    );
    progress::step("synthesize", "");
    let (nl, log) = arch_xc7::synth::synthesize(module)?;
    r.log.extend(log);
    r.stages.push(
        Json::obj()
            .with("id", "mapped_xc7")
            .with("title", "Mapped to Artix-7 cells")
            .with("kind", "netlist")
            .with("stats", stats_json(nl.stats()))
            .with("graph", graph::netlist_graph(&nl))
            .with("text", text::netlist_text(&nl)),
    );
    r.time("synthesise", t);
    if req.at(&["stop"]).as_str() == Some("synth") {
        return Ok(());
    }

    let t = Instant::now();
    let package = req.at(&["package"]).as_str().unwrap_or("");
    let device = format!("{part}{package}-1");
    let xdc = arch_xc7::xdc::Constraints::parse(pins(req))?;
    let root = xc7_root()?;
    let opts = arch_xc7::flow::FlowOptions {
        part: device.clone(),
        seed: req.at(&["seed"]).as_i64().unwrap_or(1).max(0) as u64,
        effort: req.at(&["effort"]).as_f64().unwrap_or(1.0),
        // The browser has no threads for parallel seeds.
        tries: 1,
    };
    let top = nl.top.map(|t| nl.str(t).to_string()).unwrap_or_else(|| "top".into());
    let names = nl.clone();
    let res = arch_xc7::flow::implement(nl, &root, &xdc, &opts)?;
    r.log.extend(res.log.iter().cloned());
    r.time("place and route", t);
    progress::note(|| "preparing the views".into());
    out.set(
        "placed",
        Json::obj()
            .with("slices", res.placement.slices.len())
            .with("block_rams", res.placement.rams.len())
            .with("dsps", res.placement.dsps.len())
            .with("ios", res.placement.ports.len()),
    );

    // Timing: each clock's period and critical path (cell outputs with
    // their arrival times).
    let target = req.at(&["freq"]).as_f64().filter(|f| *f > 0.0);
    let clocks: Vec<Json> = res
        .timing
        .iter()
        .map(|c| {
            // A clock is named by its input port, as the XDC names it, or
            // by the PLL making it.
            let pll = names.nets[c.net]
                .driver
                .filter(|d| names.cell_kind(d.cell) == arch_xc7::pll::PLL)
                .map(|d| names.str(names.cells[d.cell].name));
            let name = names
                .live_cells()
                .find(|&p| names.cell_kind(p) == "$input" && names.pin_net(p, "PAD") == Some(c.net))
                .map(|p| names.str(names.cells[p].name))
                .or(pll)
                .unwrap_or_else(|| names.str(names.nets[c.net].name))
                .to_string();
            let period_ns = c.period / 1000.0;
            let target_mhz = arch_xc7::flow::target_ns(&names, &xdc, c.net)
                .map(|ns| 1000.0 / ns)
                .or(target)
                .filter(|f| *f > 0.0);
            let path: Vec<Json> = c
                .path
                .iter()
                .filter_map(|step| {
                    let (pin, rest) = step.split_once(' ')?;
                    let (cell, pin) = pin.rsplit_once('.')?;
                    let ps: f64 = rest.rsplit_once('@')?.1.trim().parse().ok()?;
                    Some(Json::obj().with("cell", cell).with("pin", pin).with("t", ps / 1000.0))
                })
                .collect();
            let mut j = Json::obj()
                .with("clock", name)
                .with("period_ns", period_ns)
                .with("fmax_mhz", 1e3 / period_ns.max(1e-3))
                .with("path", path);
            if let Some(f) = target_mhz {
                j.set("target_mhz", (f * 1000.0).round() / 1000.0);
                j.set("met", period_ns <= 1000.0 / f);
            }
            j
        })
        .collect();
    out.set("timing", Json::obj().with("clocks", clocks).with("certified", false));
    let t = Instant::now();
    let bit = arch_xc7::bitstream::bit_file(
        &arch_xc7::device::Part::load(&root, &device)?,
        &top,
        &res.bitstream,
    )?;
    out.set("bin", base64(&bit));
    out.set("bin_name", format!("{top}.bit"));
    out.set("family", Family::Artix7.name());
    r.time("export", t);
    Ok(())
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
