//! Testbench sessions, for pages that drive a design cycle by cycle and read
//! it back: tests that set inputs and check outputs, waveforms, and boards
//! that run a program from a memory. Weft has no testbench constructs
//! (docs/WEFT.md §19), so the page states the stimulus.
//!
//! - `tb_start {files, top, params, clock_hz}` elaborates the design (a
//!   Weft top as an instance: its inputs synchronous to its clock, and a
//!   module without a clock port given one, `clk`) and
//!   keeps a two-state simulator of it (`undef` reads as 0, as in
//!   docs/WEFT.md §17), with every input at 0. It answers `{ok, id, ports,
//!   clocks, memories, state, signals}` (`signals` only when the request has
//!   `"signals": true`; `state` says whether the design has registers or
//!   memories; a port the testbench added has `added`), or `{ok: false,
//!   error, diagnostics}`.
//! - `tb_run {id, ops}` applies the operations in order and answers `{ok,
//!   cycle, reads}`, or `{ok: false, error, cycle, reads}` at the first
//!   operation that fails (the ones before it took effect):
//!   - `{"restart": true}` returns to power-up: every register and memory
//!     at its initial value, every input at 0, cycle 0;
//!   - `{"set": {name: value}}` drives inputs, then settles the logic;
//!   - `{"step": n, "clock": name, "trace": [names]}` runs `n` cycles (1 if
//!     absent) of the clock (the design's only clock if absent), reading
//!     the traced signals after each;
//!   - `{"read": [names]}` reads signals now;
//!   - `{"load": {"mem": name, "start": addr, "words": [values]}}` writes a
//!     memory;
//!   - `{"dump": {"mem": name, "start": addr, "count": n}}` reads one.
//!
//!   A read is `{cycle, values: {name: value}}` (a dump: `{cycle, mem,
//!   start, words}`). Values are numbers below 2⁵³ and `"0x…"` strings
//!   above, unsigned (a port's `signed` says how to read it); inputs take
//!   either form, and negative numbers for signed signals.
//! - `tb_start {bitstream, part, package, pins, flips}` starts a session on
//!   what an iCE40 bitstream (base64, as `run` gives it) implements, decoded
//!   back to its cells as `synth sim --level bitstream` does, with the pads
//!   named in the pin constraints as ports; `flips` (`[[x, y, bit], …]`,
//!   numbered as the bitstream view numbers a tile's bits) are flipped
//!   first, to see what a configuration upset does.
//! - `tb_stop {id}` drops the session.

use std::collections::HashMap;
use std::sync::Mutex;

use synth_core::{Json, Logic, LogicVec};
use synth_rtl::{Module, PortDir};
use synth_sim::program::Program;
use synth_sim::{Options, Signal, Sim};

/// Sessions kept at once; starting another drops the oldest.
const MAX_SESSIONS: usize = 16;

#[derive(Debug)]
struct Session {
    sim: Sim,
    /// The compiled design, to restart from.
    program: Program,
    /// The data inputs, set to 0 at power-up.
    inputs: Vec<Signal>,
    clocks: Vec<(String, Signal)>,
    cycle: u64,
    signals: HashMap<String, Signal>,
}

#[derive(Debug, Default)]
struct Sessions {
    next: u64,
    live: Vec<(u64, Session)>,
}

static SESSIONS: Mutex<Sessions> = Mutex::new(Sessions { next: 1, live: Vec::new() });

/// Starts a session on an elaborated module.
pub fn start(mut module: Module, want_signals: bool) -> Result<Json, String> {
    synth_sim::plls_as_wires(&mut module);
    let mut ports: Vec<(u32, String, PortDir, u32, bool)> = module
        .wires
        .iter()
        .filter_map(|w| w.port.map(|(d, pos)| (pos, w.name.clone(), d, w.width, w.signed)))
        .collect();
    ports.sort_by_key(|p| p.0);
    // A clock the testbench elaboration added (a Weft top without a clock port), and whether the design
    // holds any state at all.
    let added: Vec<String> = module
        .wires
        .iter()
        .filter(|w| w.attrs.iter().any(|(k, _)| k == "testbench_clock"))
        .map(|w| w.name.clone())
        .collect();
    let state = module.memories.iter().any(|_| true)
        || module.cells.iter().any(|c| c.alive && matches!(c.kind, synth_rtl::CellKind::Dff(_)));
    let signed: HashMap<String, bool> =
        module.wires.iter().filter(|w| w.public).map(|w| (w.name.clone(), w.signed)).collect();
    let opts = Options { four_state: false, ..Options::default() };
    let mut sim = Sim::new(&module, &opts)?;
    let clock_sigs = sim.clocks();
    let mut clocks = Vec::new();
    let mut port_json = Vec::new();
    let mut inputs = Vec::new();
    for (_, name, dir, width, signed) in &ports {
        let Some(sig) = sim.signal(name) else { continue };
        let is_clock = *dir == PortDir::Input && *width == 1 && clock_sigs.contains(&sig);
        if is_clock {
            clocks.push((name.clone(), sig.clone()));
        }
        if *dir == PortDir::Input && !is_clock {
            sim.poke(&sig, 0);
            inputs.push(sig.clone());
        }
        port_json.push(
            Json::obj()
                .with("name", name.as_str())
                .with(
                    "dir",
                    match dir {
                        PortDir::Input => "in",
                        PortDir::Output => "out",
                        PortDir::Inout => "inout",
                    },
                )
                .with("width", *width)
                .with("signed", *signed)
                .with("clock", is_clock)
                .with("added", added.contains(name)),
        );
    }
    sim.eval();
    let memories: Vec<Json> = sim
        .memory_names()
        .map(String::from)
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(|n| {
            let m = sim.memory(&n)?;
            let (width, depth, offset) = sim.memory_shape(m);
            Some(
                Json::obj()
                    .with("name", n)
                    .with("width", width)
                    .with("depth", depth)
                    .with("start", offset),
            )
        })
        .collect();
    let mut out = Json::obj()
        .with("ok", true)
        .with("ports", port_json)
        .with("clocks", Json::Arr(clocks.iter().map(|(n, _)| Json::from(n.as_str())).collect()))
        .with("memories", memories)
        .with("state", state);
    if want_signals {
        let mut names: Vec<&str> = sim.signal_names().collect();
        names.sort_unstable();
        let list: Vec<Json> = names
            .into_iter()
            .filter_map(|n| {
                let s = sim.signal(n)?;
                Some(
                    Json::obj()
                        .with("name", n)
                        .with("width", s.width())
                        .with("signed", signed.get(n).copied().unwrap_or(false)),
                )
            })
            .collect();
        out.set("signals", list);
    }
    out.set("warnings", Json::Arr(sim.warnings().iter().map(|w| Json::from(w.as_str())).collect()));
    let mut all = SESSIONS.lock().unwrap();
    let id = all.next;
    all.next += 1;
    if all.live.len() >= MAX_SESSIONS {
        all.live.remove(0);
    }
    let program = sim.program().clone();
    all.live
        .push((id, Session { sim, program, inputs, clocks, cycle: 0, signals: HashMap::new() }));
    out.set("id", id);
    Ok(out)
}

/// Drops a session.
pub fn stop(req: &Json) -> Json {
    let id = req.at(&["id"]).as_i64().unwrap_or(-1);
    SESSIONS.lock().unwrap().live.retain(|(i, _)| *i as i64 != id);
    Json::obj().with("ok", true)
}

/// Applies a request's operations to its session.
pub fn run(req: &Json) -> Json {
    let id = req.at(&["id"]).as_i64().unwrap_or(-1);
    let mut all = SESSIONS.lock().unwrap();
    let Some((_, s)) = all.live.iter_mut().find(|(i, _)| *i as i64 == id) else {
        return Json::obj().with("ok", false).with("error", "no such testbench session");
    };
    let mut reads = Vec::new();
    let mut result = Ok(());
    for op in req.at(&["ops"]).as_array() {
        result = apply(s, op, &mut reads);
        if result.is_err() {
            break;
        }
    }
    let mut out = Json::obj().with("ok", result.is_ok()).with("cycle", s.cycle);
    if let Err(e) = result {
        out.set("error", e);
    }
    out.with("reads", reads)
}

impl Session {
    fn signal(&mut self, name: &str) -> Result<Signal, String> {
        if let Some(s) = self.signals.get(name) {
            return Ok(s.clone());
        }
        let s = self.sim.signal(name).ok_or_else(|| format!("no signal `{name}`"))?;
        self.signals.insert(name.to_string(), s.clone());
        Ok(s)
    }

    fn values(&mut self, names: &[Json]) -> Result<Json, String> {
        let mut vals = Vec::with_capacity(names.len());
        for n in names {
            let name = n.as_str().ok_or("signal names are strings")?;
            let s = self.signal(name)?;
            vals.push((name.to_string(), value_json(&self.sim.peek_vec(&s))));
        }
        Ok(Json::Obj(vals))
    }
}

fn apply(s: &mut Session, op: &Json, reads: &mut Vec<Json>) -> Result<(), String> {
    if matches!(op.at(&["restart"]), Json::Bool(true)) {
        s.sim = Sim::from_program(s.program.clone());
        for i in &s.inputs {
            s.sim.poke(i, 0);
        }
        s.sim.eval();
        s.cycle = 0;
    }
    if let Json::Obj(sets) = op.at(&["set"]) {
        for (name, v) in sets {
            let sig = s.signal(name)?;
            let v = parse_value(v, sig.width()).ok_or_else(|| {
                format!("`{}` is not a value for `{name}` ({} bits)", v.dump(), sig.width())
            })?;
            s.sim.try_poke_vec(&sig, &v)?;
        }
        s.sim.eval();
    }
    if let Json::Obj(load) = op.at(&["load"]) {
        let get = |k: &str| load.iter().find(|(n, _)| n == k).map(|(_, v)| v);
        let name = get("mem").and_then(Json::as_str).ok_or("`load` needs a `mem`")?;
        let m = s.sim.memory(name).ok_or_else(|| format!("no memory `{name}`"))?;
        let (width, _, offset) = s.sim.memory_shape(m);
        let start = get("start").and_then(Json::as_i64).unwrap_or(offset);
        for (i, w) in get("words").map(Json::as_array).unwrap_or(&[]).iter().enumerate() {
            let v = parse_value(w, width as usize)
                .ok_or_else(|| format!("`{}` is not a word of `{name}`", w.dump()))?;
            s.sim.write_memory(m, start + i as i64, &v);
        }
        s.sim.eval();
    }
    if !matches!(op.at(&["step"]), Json::Null) {
        let n = op.at(&["step"]).as_f64().ok_or("`step` is a number of cycles")?;
        let n = n.max(0.0) as u64;
        let clock = match op.at(&["clock"]).as_str() {
            Some(c) => s
                .clocks
                .iter()
                .find(|(name, _)| name == c)
                .map(|(_, sig)| sig.clone())
                .ok_or_else(|| format!("no clock `{c}`"))?,
            None => match s.clocks.as_slice() {
                [(_, c)] => c.clone(),
                [] => return Err("the design has no clock to step".into()),
                _ => return Err("the design has several clocks: say which to step".into()),
            },
        };
        let trace = op.at(&["trace"]).as_array().to_vec();
        for _ in 0..n {
            s.sim.cycle(&clock);
            s.cycle += 1;
            if !trace.is_empty() {
                let values = s.values(&trace)?;
                reads.push(Json::obj().with("cycle", s.cycle).with("values", values));
            }
        }
    }
    if let Json::Arr(names) = op.at(&["read"]) {
        let values = s.values(names)?;
        reads.push(Json::obj().with("cycle", s.cycle).with("values", values));
    }
    if let Json::Obj(dump) = op.at(&["dump"]) {
        let get = |k: &str| dump.iter().find(|(n, _)| n == k).map(|(_, v)| v);
        let name = get("mem").and_then(Json::as_str).ok_or("`dump` needs a `mem`")?;
        let m = s.sim.memory(name).ok_or_else(|| format!("no memory `{name}`"))?;
        let (_, depth, offset) = s.sim.memory_shape(m);
        let start = get("start").and_then(Json::as_i64).unwrap_or(offset);
        let count = get("count").and_then(Json::as_i64).unwrap_or(depth as i64).max(0);
        let words: Vec<Json> =
            (0..count).map(|i| value_json(&s.sim.read_memory(m, start + i))).collect();
        reads.push(
            Json::obj()
                .with("cycle", s.cycle)
                .with("mem", name)
                .with("start", start)
                .with("words", words),
        );
    }
    Ok(())
}

/// A value as a number when it fits a double exactly, else hexadecimal.
fn value_json(v: &LogicVec) -> Json {
    if v.width() <= 53
        && let Some(n) = v.to_u64()
    {
        return Json::from(n as f64);
    }
    let mut s = String::from("0x");
    let digits = v.width().div_ceil(4).max(1);
    for d in (0..digits).rev() {
        let mut x = 0u32;
        for k in 0..4 {
            if v.get(d * 4 + k) == Logic::One && d * 4 + k < v.width() {
                x |= 1 << k;
            }
        }
        s.push(char::from_digit(x, 16).unwrap());
    }
    Json::Str(s)
}

/// A value of `width` bits from a number (negative ones in two's
/// complement), `true`/`false`, or a string in decimal, `0x`, `0b` or `0o`.
fn parse_value(v: &Json, width: usize) -> Option<LogicVec> {
    match v {
        Json::Bool(b) => Some(LogicVec::from_u64(u64::from(*b), width)),
        Json::Num(n) if n.fract() == 0.0 => {
            if *n < 0.0 {
                Some(LogicVec::from_i64(*n as i64, width))
            } else {
                Some(LogicVec::from_u64(*n as u64, width.max(1)).resize(width, false))
            }
        }
        Json::Str(s) => {
            let t = s.trim().replace('_', "");
            let (neg, t) = match t.strip_prefix('-') {
                Some(r) => (true, r.to_string()),
                None => (false, t),
            };
            let (radix, digits) = match t.get(..2) {
                Some("0x") | Some("0X") => (16, &t[2..]),
                Some("0b") | Some("0B") => (2, &t[2..]),
                Some("0o") | Some("0O") => (8, &t[2..]),
                _ => (10, &t[..]),
            };
            if neg {
                let n = i64::from_str_radix(digits, radix).ok()?;
                return Some(LogicVec::from_i64(-n, width));
            }
            LogicVec::parse_radix(digits, radix, width)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip() {
        assert_eq!(value_json(&LogicVec::from_u64(5, 4)), Json::from(5.0));
        let wide = parse_value(&Json::Str("0x1_0000_0000_0000_0001".into()), 72).unwrap();
        assert_eq!(value_json(&wide), Json::Str("0x010000000000000001".into()));
        let neg = parse_value(&Json::from(-1.0), 8).unwrap();
        assert_eq!(neg.to_u64(), Some(255));
        assert_eq!(parse_value(&Json::Str("-2".into()), 4).unwrap().to_u64(), Some(14));
        assert_eq!(parse_value(&Json::Bool(true), 1).unwrap().to_u64(), Some(1));
    }
}
