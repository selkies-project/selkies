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
//! The suites under `tests/` drive this server over its public interfaces.

pub mod audio;
pub mod capture;
pub mod clipboard;
pub mod commands;
pub mod core;
pub mod display;
pub mod files;
pub mod gamepad;
pub mod http;
pub mod input;
pub mod log;
pub mod server;
pub mod session_screens;
pub mod settings;
pub mod stats;
pub mod webcam;
pub mod webrtc;
pub mod wm;
pub mod ws;

/// The pixelflux and pcmflux this build links, as `selkies --version` names them.
pub fn native_libraries() -> String {
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

/// The software encoder behind each video codec of this build, `codec=library` joined by commas.
fn software_encoders() -> String {
    use pixelflux::encoders::{Codec, software_encoder};
    Codec::VIDEO
        .into_iter()
        .filter_map(|codec| {
            software_encoder(codec).map(|enc| format!("{}={}", codec.name(), enc.library))
        })
        .collect::<Vec<_>>()
        .join(",")
}
