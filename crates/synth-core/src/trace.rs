//! Traces of the flow's iterative steps, for front ends that replay them:
//! the placer records each temperature step of its annealing, the router
//! each iteration of its negotiation. Recording is off unless a caller
//! collects ([`collect`]) on the thread that runs the flow; then [`record`]
//! keeps what the step reports. Records from other threads are dropped.

use std::cell::RefCell;

/// One step of an iterative algorithm: its kind (`place`, `route`) and the
/// numbers the algorithm reports for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub kind: &'static str,
    pub values: Vec<f64>,
}

thread_local! {
    static TRACE: RefCell<Option<Vec<Record>>> = const { RefCell::new(None) };
}

/// Whether a caller is collecting on this thread.
pub fn enabled() -> bool {
    TRACE.with(|t| t.borrow().is_some())
}

/// Records a step, computing its values only when someone collects them.
pub fn record(kind: &'static str, values: impl FnOnce() -> Vec<f64>) {
    TRACE.with(|t| {
        if let Some(v) = t.borrow_mut().as_mut() {
            v.push(Record { kind, values: values() });
        }
    });
}

/// Runs `f`, collecting the records it makes on this thread.
pub fn collect<R>(f: impl FnOnce() -> R) -> (R, Vec<Record>) {
    struct Reset(Option<Vec<Record>>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TRACE.with(|t| *t.borrow_mut() = self.0.take());
        }
    }
    let reset = Reset(TRACE.with(|t| t.borrow_mut().replace(Vec::new())));
    let out = f();
    let records = TRACE.with(|t| t.borrow_mut().replace(Vec::new())).unwrap_or_default();
    drop(reset);
    (out, records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_only_while_collecting() {
        record("x", || vec![1.0]);
        let ((), recs) = collect(|| {
            assert!(enabled());
            record("place", || vec![1.0, 2.0]);
        });
        assert_eq!(recs, vec![Record { kind: "place", values: vec![1.0, 2.0] }]);
        assert!(!enabled());
    }
}
