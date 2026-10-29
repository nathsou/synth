//! Simulation of the design on a board: its clock driven, its outputs shown
//! (LEDs by their duty cycle, `tx` decoded as a serial terminal) and its
//! inputs set by the page (buttons, and `rx` from typed text), run with
//! synth-sim for as many cycles as a request allows.
//!
//! The design is elaborated with its `CLK_HZ` at [`DEFAULT_HZ`], a clock the
//! simulator can keep up with, so the examples (which derive their rates
//! from `CLK_HZ`) behave as on the board, in real time. `synth_pll` is
//! simulated as a wire, locked from the start.
//!
//! A register marked as a frame (`#[attr("frame", "WxH")]` in Weft,
//! `(* frame = "WxH" *)` in Verilog) is read directly at the end of each
//! request and shown as an image: pixel (x, y) is bits `(y·W + x)·B` to
//! `(y·W + x + 1)·B` of it, `B` (the bits per pixel) its width over `W·H`.
//! The page sees the design's picture without decoding it from the UART.

use std::collections::VecDeque;
use std::sync::Mutex;

use synth_core::Json;
use synth_core::time::Instant;
use synth_rtl::{Module, PortDir};
use synth_sim::{Options, Signal, Sim};

/// The simulated clock: 8 cycles per bit at 115200 baud.
pub const DEFAULT_HZ: u64 = 921_600;
const BAUD: u64 = 115_200;

/// An 8N1 serial receiver sampling a line once per cycle.
#[derive(Debug)]
struct Receiver {
    cpb: u64,
    prev: bool,
    /// Cycles since the start bit's falling edge, while receiving.
    at: Option<u64>,
    byte: u8,
}

impl Receiver {
    fn sample(&mut self, line: bool) -> Option<u8> {
        let mut out = None;
        match self.at {
            None => {
                if self.prev && !line {
                    self.at = Some(0);
                    self.byte = 0;
                }
            }
            Some(t) => {
                let t = t + 1;
                self.at = Some(t);
                // The middle of data bit k is 1.5 + k bit times after the
                // start bit's edge; the stop bit's middle ends the byte.
                let half = self.cpb / 2;
                if t >= half + self.cpb && (t - half).is_multiple_of(self.cpb) {
                    let k = (t - half) / self.cpb - 1;
                    if k < 8 {
                        self.byte |= u8::from(line) << k;
                    } else {
                        out = line.then_some(self.byte);
                        self.at = None;
                    }
                }
            }
        }
        self.prev = line;
        out
    }
}

/// An 8N1 serial transmitter driving a line once per cycle.
#[derive(Debug)]
struct Transmitter {
    cpb: u64,
    queue: VecDeque<u8>,
    /// The frame being sent (start, 8 data bits LSB first, stop) and the
    /// cycles into it.
    frame: Option<(u16, u64)>,
}

impl Transmitter {
    fn line(&mut self) -> bool {
        if self.frame.is_none() {
            // Idle: high.
            let Some(b) = self.queue.pop_front() else { return true };
            self.frame = Some((u16::from(b) << 1 | 1 << 9, 0));
        }
        let (bits, t) = self.frame.as_mut().unwrap();
        let level = *bits >> (*t / self.cpb) & 1 != 0;
        *t += 1;
        if *t >= 10 * self.cpb {
            self.frame = None;
        }
        level
    }
}

/// A register shown as an image.
#[derive(Debug)]
struct Frame {
    name: String,
    sig: Signal,
    width: usize,
    height: usize,
    bpp: usize,
    /// The pixels last reported, and how many times they changed.
    last: Vec<u64>,
    changes: u64,
}

impl Frame {
    fn pixels(&self, sim: &Sim) -> Vec<u64> {
        let v = sim.peek_vec(&self.sig);
        (0..self.width * self.height)
            .map(|i| {
                (0..self.bpp).fold(0, |p, k| {
                    p | u64::from(v.get(i * self.bpp + k) == synth_core::Logic::One) << k
                })
            })
            .collect()
    }
}

/// Parses a frame's size, `WxH`.
fn frame_size(s: &str) -> Option<(usize, usize)> {
    let (w, h) = s.trim().split_once(['x', 'X', '×'])?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// A port of the design shown or driven by the page.
#[derive(Debug)]
struct Port {
    name: String,
    sig: Signal,
    width: usize,
    /// Cycles each bit was high since the last report (outputs).
    on: Vec<u64>,
}

#[derive(Debug)]
pub struct Session {
    sim: Sim,
    hz: u64,
    clock: Signal,
    tx: Option<(Signal, Receiver)>,
    rx: Option<(Signal, Transmitter)>,
    outputs: Vec<Port>,
    inputs: Vec<Port>,
    frames: Vec<Frame>,
    cycles: u64,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// Starts a simulation of an elaborated design, replacing any other.
pub fn start(mut module: Module, hz: u64) -> Result<Json, String> {
    synth_sim::plls_as_wires(&mut module);
    let mut ports: Vec<(u32, String, PortDir, usize)> = module
        .wires
        .iter()
        .filter_map(|w| w.port.map(|(d, pos)| (pos, w.name.clone(), d, w.width as usize)))
        .collect();
    ports.sort_by_key(|p| p.0);
    let marked: Vec<(String, usize, String)> = module
        .wires
        .iter()
        .filter_map(|w| {
            w.attrs.iter().find_map(|(k, v)| match (k.as_str(), v) {
                ("frame", synth_rtl::Attr::Str(size)) => {
                    Some((w.name.clone(), w.width as usize, size.clone()))
                }
                _ => None,
            })
        })
        .collect();
    let sim = Sim::new(&module, &Options::default())?;
    let clocks = sim.clocks();
    let signal = |name: &str| sim.signal(name).ok_or_else(|| format!("no signal {name}"));
    let mut clock = None;
    for (_, name, dir, width) in &ports {
        if *dir == PortDir::Input && *width == 1 {
            let s = signal(name)?;
            if clocks.contains(&s) {
                clock = Some(s);
                break;
            }
        }
    }
    let clock = match clock {
        Some(c) => c,
        None => ["clk", "clock"]
            .iter()
            .find_map(|n| sim.signal(n))
            .ok_or("the design has no clock input to drive")?,
    };
    let cpb = (hz / BAUD).max(1);
    let (mut outputs, mut inputs) = (Vec::new(), Vec::new());
    let (mut tx, mut rx) = (None, None);
    for (_, name, dir, width) in ports {
        let sig = signal(&name)?;
        if sig == clock {
            continue;
        }
        match (dir, name.as_str(), width) {
            (PortDir::Output, "tx", 1) => {
                tx = Some((sig, Receiver { cpb, prev: true, at: None, byte: 0 }));
            }
            (PortDir::Input, "rx", 1) => {
                rx = Some((sig, Transmitter { cpb, queue: VecDeque::new(), frame: None }));
            }
            (PortDir::Output, _, _) => {
                outputs.push(Port { name, sig, width, on: vec![0; width] });
            }
            (PortDir::Input, _, _) => inputs.push(Port { name, sig, width, on: Vec::new() }),
            _ => {}
        }
    }
    let (mut frames, mut warnings) = (Vec::new(), Vec::new());
    for (name, width, text) in marked {
        let Some((w, h)) = frame_size(&text) else {
            warnings.push(format!("frame `{name}`: `{text}` is not a size like `8x8`"));
            continue;
        };
        if width % (w * h) != 0 {
            warnings.push(format!("frame `{name}`: its {width} bits are not {w}×{h} pixels"));
            continue;
        }
        let Some(sig) = sim.signal(&name) else { continue };
        frames.push(Frame {
            name,
            sig,
            width: w,
            height: h,
            bpp: width / (w * h),
            last: Vec::new(),
            changes: 0,
        });
    }
    let mut s = Session { sim, hz, clock, tx, rx, outputs, inputs, frames, cycles: 0 };
    // Inputs start low, a serial line idle (high).
    for p in &s.inputs {
        s.sim.poke(&p.sig, 0);
    }
    if let Some((sig, _)) = &s.rx {
        s.sim.poke(sig, 1);
    }
    s.sim.eval();
    let port = |p: &Port| Json::obj().with("name", p.name.as_str()).with("width", p.width);
    let info = Json::obj()
        .with("hz", hz)
        .with("outputs", Json::Arr(s.outputs.iter().map(port).collect()))
        .with("inputs", Json::Arr(s.inputs.iter().map(port).collect()))
        .with("uart_tx", s.tx.is_some())
        .with("uart_rx", s.rx.is_some())
        .with(
            "frames",
            Json::Arr(
                s.frames
                    .iter()
                    .map(|f| {
                        Json::obj()
                            .with("name", f.name.as_str())
                            .with("width", f.width)
                            .with("height", f.height)
                            .with("bpp", f.bpp)
                    })
                    .collect(),
            ),
        )
        .with(
            "warnings",
            Json::Arr(
                s.sim.warnings().iter().chain(&warnings).map(|w| Json::from(w.as_str())).collect(),
            ),
        );
    *SESSION.lock().unwrap() = Some(s);
    Ok(info)
}

/// Runs the simulation: `{inputs: {name: value}, rx: [bytes], cycles,
/// budget_ms}` sets the inputs, queues the bytes on `rx`, then runs up to
/// `cycles` cycles or `budget_ms` milliseconds. Returns the cycles run, the
/// simulated time, the bytes received on `tx`, each output's value and
/// the share of the cycles each of its bits was high, and each frame's
/// pixels (only when they changed since the last request).
pub fn step(req: &Json) -> Result<Json, String> {
    let mut guard = SESSION.lock().unwrap();
    let s = guard.as_mut().ok_or("no simulation running")?;
    if let Json::Obj(inputs) = req.at(&["inputs"]) {
        for (name, v) in inputs {
            if let Some(p) = s.inputs.iter().find(|p| p.name == *name) {
                let v = v.as_f64().unwrap_or(0.0) as u64;
                s.sim.poke(&p.sig, v);
            }
        }
    }
    if let Some((_, t)) = &mut s.rx {
        for b in req.at(&["rx"]).as_array() {
            t.queue.push_back(b.as_f64().unwrap_or(0.0) as u8);
        }
    }
    let max = req.at(&["cycles"]).as_f64().unwrap_or(1e6).max(0.0) as u64;
    let budget = req.at(&["budget_ms"]).as_f64().unwrap_or(50.0);
    let start = Instant::now();
    let mut received = Vec::new();
    let mut run = 0u64;
    for p in &mut s.outputs {
        p.on.iter_mut().for_each(|c| *c = 0);
    }
    while run < max {
        if run.is_multiple_of(256) && run > 0 && start.elapsed().as_secs_f64() * 1000.0 >= budget {
            break;
        }
        if let Some((sig, t)) = &mut s.rx {
            let level = t.line();
            s.sim.poke(sig, u64::from(level));
        }
        s.sim.cycle(&s.clock);
        run += 1;
        if let Some((sig, r)) = &mut s.tx
            && let Some(b) = r.sample(s.sim.peek(sig) == Some(1))
        {
            received.push(Json::from(u64::from(b)));
        }
        for p in &mut s.outputs {
            let v = s.sim.peek(&p.sig).unwrap_or(0);
            for (k, c) in p.on.iter_mut().enumerate() {
                *c += v >> k & 1;
            }
        }
    }
    s.cycles += run;
    let outputs = s
        .outputs
        .iter()
        .map(|p| {
            let duty: Vec<Json> =
                p.on.iter()
                    .map(|&c| Json::from(if run == 0 { 0.0 } else { c as f64 / run as f64 }))
                    .collect();
            Json::obj()
                .with("name", p.name.as_str())
                .with("value", s.sim.peek(&p.sig).map_or(Json::Null, Json::from))
                .with("duty", Json::Arr(duty))
        })
        .collect();
    let frames = s
        .frames
        .iter_mut()
        .map(|f| {
            let px = f.pixels(&s.sim);
            let o = Json::obj().with("name", f.name.as_str());
            if px == f.last {
                return o.with("changes", f.changes);
            }
            f.changes += 1;
            f.last = px;
            o.with("changes", f.changes)
                .with("pixels", Json::Arr(f.last.iter().map(|&p| Json::from(p)).collect()))
        })
        .collect();
    Ok(Json::obj()
        .with("cycles", run)
        .with("total", s.cycles)
        .with("time", s.cycles as f64 / s.hz as f64)
        .with("tx", Json::Arr(received))
        .with("outputs", Json::Arr(outputs))
        .with("frames", Json::Arr(frames)))
}

/// Ends the simulation.
pub fn stop() {
    *SESSION.lock().unwrap() = None;
}
