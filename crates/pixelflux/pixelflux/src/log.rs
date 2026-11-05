//! The verbosity switch behind every backend's tagged line.
//!
//! What an operator reads back from a user is printed plainly: one line per
//! capture start, encoder choice, zero-copy or readback decision, and fallback,
//! tagged `[X11]`, `[Wayland]`, `[HostCapture]`, or `[pixelflux]` on stdout,
//! with warnings and errors on stderr. The mechanics behind those lines (each
//! dmabuf import, the CUDA device enumeration, the per-second rate counters,
//! an in-place reconfigure) go through `debug!` and print only once the
//! switch is on: a capture whose `CaptureSettings.debug_logging` is set turns
//! it on for the process (selkies passes its own `--debug` there), and
//! `PIXELFLUX_DEBUG=1` in the environment does the same for standalone use.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);
static ENV_DEBUG: OnceLock<bool> = OnceLock::new();

/// Turn the debug lines on or off for the process.
pub fn set_debug(on: bool) {
    DEBUG.store(on, Ordering::Relaxed);
}

/// Whether debug lines print: the last capture's setting, or `PIXELFLUX_DEBUG`.
pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
        || *ENV_DEBUG.get_or_init(|| {
            std::env::var_os("PIXELFLUX_DEBUG").is_some_and(|v| !v.is_empty() && v != "0")
        })
}

/// A `println!` that prints only while `debug_enabled`.
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::log::debug_enabled() {
            println!($($arg)*);
        }
    };
}
pub(crate) use debug;
