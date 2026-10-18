//! Wall-clock timing for logs that also works on WebAssembly.
//!
//! `std::time::Instant::now()` panics on `wasm32-unknown-unknown`. There,
//! [`Instant`] reads the clock through an import, `env.synth_now_ms`
//! (milliseconds, for example `performance.now()`), which the page that
//! instantiates the module provides.

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;

#[cfg(target_arch = "wasm32")]
pub use wasm::Instant;

#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
mod wasm {
    use std::time::Duration;

    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        fn synth_now_ms() -> f64;
    }

    fn now_ms() -> f64 {
        // SAFETY: the import takes no arguments and returns a number.
        unsafe { synth_now_ms() }
    }

    #[derive(Copy, Clone, Debug)]
    pub struct Instant(f64);

    impl Instant {
        pub fn now() -> Instant {
            Instant(now_ms())
        }

        pub fn elapsed(&self) -> Duration {
            Duration::from_secs_f64(((now_ms() - self.0) / 1000.0).max(0.0))
        }
    }
}
