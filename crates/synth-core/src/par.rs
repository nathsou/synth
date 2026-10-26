//! Deterministic parallel maps: work spread over threads, results in input
//! order, so that nothing downstream depends on the number of threads or
//! on scheduling.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The threads parallel work may use: the machine's, up to `limit` (one
/// where threads cannot be spawned, as in WebAssembly).
pub fn threads(limit: usize) -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, limit.max(1))
}

/// `f` applied to every item, on up to `threads` threads taking items in
/// turn; the results come back in the order of `items`.
pub fn map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = threads.min(items.len());
    if threads <= 1 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    *results[i].lock().expect("result slot") = Some(r);
                }
            });
        }
    });
    results
        .into_iter()
        .map(|r| r.into_inner().expect("result slot").expect("every item mapped"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_keep_input_order() {
        let items: Vec<u64> = (0..100).collect();
        for t in [1, 2, 7] {
            assert_eq!(map(&items, t, |&x| x * x), items.iter().map(|x| x * x).collect::<Vec<_>>());
        }
        assert!(map(&[] as &[u8], 4, |&x| x).is_empty());
    }
}
