//! The synth playground: the whole flow, from source to bitstream, as data
//! for the web app in `web/`.
//!
//! [`handle`] answers the app's requests (JSON in, JSON out). It runs in the
//! browser as WebAssembly (see `wasm`), or behind the small HTTP server of
//! `synth playground` (see [`server`]).
//!
//! - `run`: elaborates, synthesises, places, routes and generates the
//!   bitstream, and returns every stage: diagnostics, the design as a graph
//!   after each pass, the packed netlist, the placed and routed die, timing
//!   and the decoded configuration bits.
//! - `analyze`: diagnostics and the name index of Weft sources, for the
//!   editor (hover, go to definition).
//! - `format`: a Weft source, formatted.
//! - `examples`, `devices`: what the app offers to start from.

pub mod chip;
pub mod graph;
#[cfg(not(target_arch = "wasm32"))]
pub mod server;
#[cfg(target_arch = "wasm32")]
mod wasm;

use synth_core::time::Instant;
use synth_core::{Diagnostics, FileId, Json, Severity, SourceMap};

/// A ready-made design.
#[derive(Debug)]
pub struct Example {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub files: &'static [(&'static str, &'static str)],
    pub pcf: &'static str,
    pub part: &'static str,
    pub package: &'static str,
}

const GO_BOARD_PCF: &str = include_str!("../../../examples/weft/go_board.pcf");
const BLINKY_RAM_PCF: &str = include_str!("../../../tests/designs/blinky_ram/go_board.pcf");
const UART: &str = include_str!("../../../examples/weft/uart.weft");

pub const EXAMPLES: &[Example] = &[
    Example {
        id: "blinky",
        title: "Blinky",
        description: "A counter on four LEDs",
        files: &[("blinky.weft", include_str!("../../../examples/weft/blinky.weft"))],
        pcf: GO_BOARD_PCF,
        part: "hx1k",
        package: "vq100",
    },
    Example {
        id: "hello",
        title: "Hello UART",
        description: "Prints a greeting over a UART once a second",
        files: &[
            ("hello.weft", include_str!("../../../examples/weft/hello.weft")),
            ("uart.weft", UART),
        ],
        pcf: GO_BOARD_PCF,
        part: "hx1k",
        package: "vq100",
    },
    Example {
        id: "echo",
        title: "UART echo",
        description: "Echoes a UART through a FIFO in block RAM",
        files: &[
            ("echo.weft", include_str!("../../../examples/weft/echo.weft")),
            ("uart.weft", UART),
        ],
        pcf: GO_BOARD_PCF,
        part: "hx1k",
        package: "vq100",
    },
    Example {
        id: "blinky_ram",
        title: "Blinky RAM",
        description: "LEDs driven through an initialized block RAM",
        files: &[("blinky_ram.weft", include_str!("../../../examples/weft/blinky_ram.weft"))],
        pcf: BLINKY_RAM_PCF,
        part: "hx1k",
        package: "vq100",
    },
    Example {
        id: "blinky_ram_v",
        title: "Blinky RAM (Verilog)",
        description: "The Verilog original of Blinky RAM",
        files: &[("blinky_ram.v", include_str!("../../../tests/designs/blinky_ram/blinky_ram.v"))],
        pcf: BLINKY_RAM_PCF,
        part: "hx1k",
        package: "vq100",
    },
];

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
                    .with("pcf", e.pcf)
                    .with("part", e.part)
                    .with("package", e.package)
            })
            .collect(),
    )
}

/// The supported parts and their packages.
pub fn devices() -> Json {
    use arch_ice40::arch::Part;
    Json::Arr(
        Part::ALL
            .iter()
            .map(|p| {
                let db = arch_ice40::chipdb::ChipDb::get(p.die());
                let pk: Vec<Json> = db
                    .packages
                    .iter()
                    .filter(|k| arch_ice40::arch::Arch::new(*p, &k.name).is_ok())
                    .map(|k| k.name.as_str().into())
                    .collect();
                Json::obj().with("part", p.name()).with("die", p.die().name()).with("packages", pk)
            })
            .collect(),
    )
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
    synth_weft::Options { top, params, ..Default::default() }
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

fn run_inner(req: &Json, r: &mut Run, out: &mut Json) -> Result<(), String> {
    // Elaboration.
    let t = Instant::now();
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
        let opts = synth_elab::ElabOptions { top, top_params: Vec::new() };
        let (m, d) =
            synth_elab::elaborate(src.as_ref().unwrap(), &sm, &opts, &synth_elab::NoBlackBoxes);
        diags.extend(d);
        m
    };
    out.set("diagnostics", diagnostics_json(&sm, &diags));
    r.time("elaborate", t);
    let Some(module) = module else {
        return Err(format!("{} error(s)", diags.error_count().max(1)));
    };
    out.set("top", module.name.as_str());

    // Synthesis, observed pass by pass.
    let t = Instant::now();
    let stages = &mut r.stages;
    let mut observe = |s: arch_ice40::synth::SynthStage| match s {
        arch_ice40::synth::SynthStage::Rtl(name, m) => {
            let id = name.replace(' ', "_");
            stages.push(
                Json::obj()
                    .with("id", id)
                    .with("title", format!("RTL, {name}"))
                    .with("kind", "rtl")
                    .with("stats", stats_json(m.stats()))
                    .with("graph", graph::rtl_graph(m)),
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
            stages.push(j);
        }
        arch_ice40::synth::SynthStage::Mapped(nl) => {
            stages.push(
                Json::obj()
                    .with("id", "mapped")
                    .with("title", "Mapped to iCE40 cells")
                    .with("kind", "netlist")
                    .with("stats", stats_json(nl.stats()))
                    .with("graph", graph::netlist_graph(nl)),
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
    let part: arch_ice40::arch::Part = req.at(&["part"]).as_str().unwrap_or("hx1k").parse()?;
    let package = req.at(&["package"]).as_str().unwrap_or("vq100").to_string();
    let pcf = arch_ice40::pcf::parse_pcf(req.at(&["pcf"]).as_str().unwrap_or(""))?;
    let mut clocks = pcf.clocks.clone();
    clocks.default_mhz = req.at(&["freq"]).as_f64().filter(|f| *f > 0.0);
    let opts = arch_ice40::flow::FlowOptions {
        part,
        package,
        seed: req.at(&["seed"]).as_i64().unwrap_or(1).max(0) as u64,
        place_effort: req.at(&["effort"]).as_f64().unwrap_or(1.0),
        timing_weight: req.at(&["timing_weight"]).as_f64().unwrap_or(0.5).clamp(0.0, 1.0),
        clocks,
        route_timing_rounds: 0,
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
    r.stages.push(
        Json::obj()
            .with("id", "packed")
            .with("title", "Packed into logic cells")
            .with("kind", "netlist")
            .with("stats", stats_json(imp.netlist.stats()))
            .with("graph", graph::netlist_graph(&imp.netlist)),
    );
    let t = Instant::now();
    let (chip, timing) = chip::chip(&imp, &opts);
    out.set("chip", chip);
    out.set("timing", timing.with("met", res.timing_met));
    out.set("bitstream", chip::bitstream(&imp.arch, &res.config));
    out.set("bin", base64(&res.config.to_bin()));
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
