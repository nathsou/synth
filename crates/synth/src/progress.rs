//! The progress of a build on the last line of a terminal: the step of the
//! flow, a bar for the whole run, what the step is doing and the time so
//! far, redrawn as the flow reports and every quarter of a second. Log
//! lines (`info!`) print above it. Nothing is drawn unless stderr is a
//! terminal.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use synth_core::progress::{self, Progress};

/// The steps of each family's flow in order, with their usual share of a
/// run's time (as in the playground's `progress.js`).
const ICE40: &[(&str, f64)] = &[
    ("elaborate", 1.0),
    ("synthesize", 20.0),
    ("device", 1.0),
    ("pack", 1.0),
    ("place", 50.0),
    ("route", 22.0),
    ("timing", 1.0),
    ("bitstream", 4.0),
];
const XC7: &[(&str, f64)] = &[
    ("elaborate", 1.0),
    ("synthesize", 25.0),
    ("pack", 1.0),
    ("device", 8.0),
    ("place", 5.0),
    ("route", 55.0),
    ("bitstream", 3.0),
    ("timing", 2.0),
];

fn label(step: &str) -> &'static str {
    match step {
        "elaborate" => "Elaborating",
        "synthesize" => "Synthesising",
        "device" => "Loading device",
        "pack" => "Packing",
        "place" => "Placing",
        "route" => "Routing",
        "timing" => "Timing",
        "bitstream" => "Bitstream",
        _ => "Working",
    }
}

struct State {
    steps: &'static [(&'static str, f64)],
    start: Instant,
    current: Option<Progress>,
    frame: usize,
    done: bool,
}

/// Runs `f` with its progress drawn on stderr, if stderr is a terminal;
/// `xc7` selects the Artix-7 flow's steps.
pub fn run<R>(xc7: bool, f: impl FnOnce() -> R) -> R {
    let err = std::io::stderr();
    if !err.is_terminal() || std::env::var("TERM").is_ok_and(|t| t == "dumb") {
        return f();
    }
    let state = Arc::new(Mutex::new(State {
        steps: if xc7 { XC7 } else { ICE40 },
        start: Instant::now(),
        current: None,
        frame: 0,
        done: false,
    }));
    let width = terminal_width();
    // The elapsed time moves on between reports (synthesis reports little).
    let ticker = {
        let state = state.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let mut s = state.lock().unwrap();
                if s.done {
                    break;
                }
                if s.current.is_some() {
                    draw(&mut s, width);
                }
            }
        })
    };
    let sink = {
        let state = state.clone();
        move |p: &Progress| {
            let mut s = state.lock().unwrap();
            s.current = Some(p.clone());
            draw(&mut s, width);
        }
    };
    let out = progress::observe(sink, f);
    state.lock().unwrap().done = true;
    let _ = ticker.join();
    let mut err = err.lock();
    let _ = write!(err, "\r\x1b[2K");
    let _ = err.flush();
    progress::set_drawn(false);
    out
}

fn draw(s: &mut State, width: usize) {
    let Some(p) = &s.current else { return };
    let total: f64 = s.steps.iter().map(|(_, w)| w).sum();
    let at = s.steps.iter().position(|(k, _)| *k == p.step);
    let mut done = 0.0;
    if let Some(at) = at {
        done += s.steps[..at].iter().map(|(_, w)| w).sum::<f64>();
        done += s.steps[at].1 * p.fraction.unwrap_or(0.0);
    }
    let fraction = done / total;
    s.frame += 1;
    const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let spin = SPINNER[s.frame % SPINNER.len()];
    let step = match at {
        Some(i) => format!("{} {}/{}", label(p.step), i + 1, s.steps.len()),
        None => label(p.step).to_string(),
    };
    let secs = s.start.elapsed().as_secs_f64();
    let time = if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}m{:02}s", secs as u64 / 60, secs as u64 % 60)
    };
    // The bar: eighths of a cell, filled in the accent colour.
    const BAR: usize = 24;
    let eighths = (fraction * (BAR * 8) as f64).round() as usize;
    let mut bar = String::new();
    for i in 0..BAR {
        let n = eighths.saturating_sub(i * 8).min(8);
        bar.push(if n == 0 {
            ' '
        } else {
            [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'][n]
        });
    }
    // Visible columns: spinner, step, bar, percentage and time; the detail
    // takes what is left of the line, so that it never wraps.
    let fixed = 2 + 18 + 1 + BAR + 2 + 5 + time.len() + 3;
    let room = width.saturating_sub(fixed + 1);
    let detail: String = p.detail.chars().take(room).collect();
    let line = format!(
        "\x1b[36m{spin}\x1b[0m \x1b[1m{step:<18}\x1b[0m \x1b[36m▕{bar}▏\x1b[0m {:>3.0}%  \x1b[2m{time}\x1b[0m{}{detail}",
        fraction * 100.0,
        if detail.is_empty() { "" } else { "  " },
    );
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\r\x1b[2K{line}");
    let _ = err.flush();
    progress::set_drawn(true);
}

/// The terminal's width: `COLUMNS`, else `stty size`, else 80.
fn terminal_width() -> usize {
    if let Some(n) = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()) {
        return n;
    }
    std::fs::File::open("/dev/tty")
        .ok()
        .and_then(|tty| std::process::Command::new("stty").arg("size").stdin(tty).output().ok())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(80)
}
