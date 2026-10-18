//! Wall-clock timing for logs that also works on WebAssembly.
//!
//! `std::time::Instant::now()` panics on `wasm32-unknown-unknown`; there,
//! [`Instant`] measures nothing and every elapsed time is zero.

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;

#[cfg(target_arch = "wasm32")]
#[derive(Copy, Clone, Debug)]
pub struct Instant;

#[cfg(target_arch = "wasm32")]
impl Instant {
    pub fn now() -> Instant {
        Instant
    }

    pub fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}
