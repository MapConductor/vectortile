//! A stopwatch that survives platforms with no clock.
//!
//! `std::time::Instant::now()` **panics** on `wasm32-unknown-unknown`: there is
//! no system clock behind it, and the standard library says so by trapping.
//! That took the whole label pass down in the browser, from a line that only
//! existed to report how long the pass took.
//!
//! The timings in this crate are instrumentation. They are reported alongside a
//! tile so a slow device can be measured; nothing decides anything from them.
//! So on the web they read as zero, and Android and iOS keep the real numbers.

/// Measures elapsed milliseconds from when it was started.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy)]
pub struct Stopwatch(std::time::Instant);

/// The same stopwatch where there is no clock to read. Always zero.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy)]
pub struct Stopwatch;

impl Stopwatch {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn start() -> Stopwatch {
        Stopwatch(std::time::Instant::now())
    }

    #[cfg(target_arch = "wasm32")]
    pub fn start() -> Stopwatch {
        Stopwatch
    }

    /// Milliseconds since [`Stopwatch::start`], or zero with no clock.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn ms(&self) -> f32 {
        self.0.elapsed().as_secs_f32() * 1000.0
    }

    #[cfg(target_arch = "wasm32")]
    pub fn ms(&self) -> f32 {
        0.0
    }
}
