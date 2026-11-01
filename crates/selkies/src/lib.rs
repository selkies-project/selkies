/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! # selkies
//!
//! The Selkies streaming server: a Linux desktop captured and encoded by `pixelflux`, its
//! sound by `pcmflux`, and streamed to a browser over WebSockets or WebRTC, with the
//! browser's keyboard, mouse, touch, gamepads, clipboard, camera, and microphone carried
//! back. The crate links both capture libraries directly as Rust, with no interpreter in
//! the process, and `main.rs` is the `selkies` executable.
//!
//! The server is being ported module by module from the Python implementation under
//! `src/selkies`, whose module docstrings are the specification each Rust module is held
//! to; the suites under `tests/` drive either server unchanged.

/// The capture stack this build links, as the startup line names it.
pub fn capture_stack() -> String {
    format!(
        "pixelflux {} (software {}), pcmflux {}",
        pixelflux_version(),
        software_encoders(),
        pcmflux_version()
    )
}

/// The pixelflux crate version compiled in.
fn pixelflux_version() -> &'static str {
    pixelflux::VERSION
}

/// The pcmflux crate version compiled in.
fn pcmflux_version() -> &'static str {
    pcmflux::VERSION
}

/// The software encoder behind each video codec of this build, `codec=library` joined by
/// commas, as `pixelflux.SOFTWARE_ENCODERS` lists them to Python.
fn software_encoders() -> String {
    use pixelflux::encoders::{Codec, software_encoder};
    Codec::VIDEO
        .into_iter()
        .filter_map(|codec| software_encoder(codec).map(|enc| format!("{}={}", codec.name(), enc.library)))
        .collect::<Vec<_>>()
        .join(",")
}
