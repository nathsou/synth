//! Progress of a run, for front ends that show it while the flow works.
//!
//! The flow names the step it is in ([`step`]) and, where it can tell, how
//! far through it is ([`advance`]). A front end installs a sink on the
//! thread that runs the flow ([`observe`]); it receives the steps as they
//! start and their advances at most every [`INTERVAL_MS`] milliseconds.
//! Without a sink, reporting costs a thread-local read. Reports from other
//! threads (routing workers) are dropped.
//!
//! A terminal front end may draw the progress on the last line of stderr
//! ([`set_drawn`]); the flow prints its log lines through [`info`], which
//! clears that line first.

use crate::time::Instant;
use std::cell::RefCell;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// The steps of the flow, in order, as front ends know them.
pub const STEPS: &[&str] =
    &["elaborate", "synthesize", "pack", "device", "place", "route", "timing", "bitstream"];

/// The least time between two reports within a step.
pub const INTERVAL_MS: f64 = 100.0;

/// Where the flow is.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// One of [`STEPS`].
    pub step: &'static str,
    /// How far through the step, from 0 to 1, if known.
    pub fraction: Option<f64>,
    /// What the step is doing, for people ("iteration 4: 120 congested
    /// wires").
    pub detail: String,
}

struct Observer {
    sink: Box<dyn FnMut(&Progress)>,
    current: Progress,
    /// The detail of the last step or advance, which notes add to.
    base: String,
    last: Option<Instant>,
}

thread_local! {
    static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
}

/// Runs `f` with `sink` receiving its progress on this thread.
pub fn observe<R>(sink: impl FnMut(&Progress) + 'static, f: impl FnOnce() -> R) -> R {
    struct Reset(Option<Observer>);
    impl Drop for Reset {
        fn drop(&mut self) {
            OBSERVER.with(|o| *o.borrow_mut() = self.0.take());
        }
    }
    let observer = Observer {
        sink: Box::new(sink),
        current: Progress { step: STEPS[0], fraction: None, detail: String::new() },
        base: String::new(),
        last: None,
    };
    let _reset = Reset(OBSERVER.with(|o| o.borrow_mut().replace(observer)));
    f()
}

/// Whether a sink is listening on this thread.
pub fn observed() -> bool {
    OBSERVER.with(|o| o.borrow().is_some())
}

/// Starts `step` (one of [`STEPS`]), reported at once.
pub fn step(step: &'static str, detail: &str) {
    debug_assert!(STEPS.contains(&step), "unknown step {step}");
    update(true, |o| {
        o.current.step = step;
        o.current.fraction = None;
        o.current.detail = detail.to_string();
        o.base = detail.to_string();
    });
}

/// Reports how far through the current step the flow is, from 0 to 1;
/// `detail` is only called when the report goes out.
pub fn advance(fraction: f64, detail: impl FnOnce() -> String) {
    let due = OBSERVER.with(|o| {
        o.borrow().as_ref().is_some_and(|o| {
            o.last.is_none_or(|t| t.elapsed().as_secs_f64() * 1000.0 >= INTERVAL_MS)
        })
    });
    if due {
        let detail = detail();
        update(false, |o| {
            o.current.fraction = Some(fraction.clamp(0.0, 1.0));
            o.base.clone_from(&detail);
            o.current.detail = detail;
        });
    }
}

/// Reports what the current step is doing, after the detail of the last
/// [`step`] or [`advance`], without changing how far through it is.
pub fn note(detail: impl FnOnce() -> String) {
    let due = OBSERVER.with(|o| {
        o.borrow().as_ref().is_some_and(|o| {
            o.last.is_none_or(|t| t.elapsed().as_secs_f64() * 1000.0 >= INTERVAL_MS)
        })
    });
    if due {
        let detail = detail();
        update(false, |o| {
            o.current.detail =
                if o.base.is_empty() { detail } else { format!("{} · {detail}", o.base) };
        });
    }
}

static DRAWN: AtomicBool = AtomicBool::new(false);

/// Marks whether a progress line is drawn at the end of stderr (without a
/// newline); call it while holding the stderr lock that drew it.
pub fn set_drawn(drawn: bool) {
    DRAWN.store(drawn, Ordering::Relaxed);
}

/// Prints `info: <line>` to stderr, clearing any progress line first (the
/// front end draws it again on its next report).
pub fn info(line: &str) {
    eprint(&format!("info: {line}\n"));
}

/// Prints `text` to stderr, clearing any progress line first.
pub fn eprint(text: &str) {
    let mut err = std::io::stderr().lock();
    if DRAWN.swap(false, Ordering::Relaxed) {
        let _ = write!(err, "\r\x1b[2K");
    }
    let _ = write!(err, "{text}");
}

fn update(now: bool, change: impl FnOnce(&mut Observer)) {
    // The sink is taken out while it runs, so that it may report too.
    let Some(mut o) = OBSERVER.with(|o| o.borrow_mut().take()) else { return };
    change(&mut o);
    if now || o.last.is_none_or(|t| t.elapsed().as_secs_f64() * 1000.0 >= INTERVAL_MS) {
        (o.sink)(&o.current);
        o.last = Some(Instant::now());
    }
    OBSERVER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(o);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn steps_go_out_at_once_and_advances_at_most_every_interval() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        observe(
            move |p| s.lock().unwrap().push((p.step, p.fraction)),
            || {
                step("place", "");
                for i in 0..1000 {
                    advance(i as f64 / 1000.0, String::new);
                }
                step("route", "");
            },
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.first(), Some(&("place", None)));
        assert_eq!(seen.last(), Some(&("route", None)));
        // A thousand quick advances are not a thousand reports.
        assert!(seen.len() < 10, "{seen:?}");
        // Nothing is observed outside `observe`.
        assert!(!observed());
        step("place", "");
    }
}
