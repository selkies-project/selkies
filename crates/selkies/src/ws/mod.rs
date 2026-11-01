/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The WebSockets data plane: per-client connection handling (roles, input dispatch,
//! settings), the per-display video relays with their bounded backpressure and decode gate,
//! the shared audio fan-out, microphone forwarding, clipboard and cursor delivery, stats, and
//! the display owner model.
//!
//! The plane is one `WsService` the HTTP router mounts at `GET /api/websockets`
//! (`handler::websocket_handler`). Each connection runs `handler::run_socket`: the
//! handshake pushes (`AUTH_SUCCESS`, `MODE websockets`, `MK_ACCESS`, `DISPLAY_CONFIG_UPDATE`,
//! `cursor`, `server_settings`), the message loop, and the teardown behind the reconnect
//! grace. Every socket's frames leave through one writer under the liveness rule of
//! `client::Client::send_live`; the capture of each display fans out to a `relay::VideoRelay`
//! per socket; the owner's acks run `link::DisplayLink`'s gate and steer; the pcmflux frames
//! fan out through `audio::run_fanout`. The wire itself is `protocol`, the traits the
//! integrator wires are `host` (implemented over the landed modules in `wiring`), and the
//! server's state and the orchestration of captures, settings, and the owner model are `state`
//! and `server`.
//!
//! Threading model: tokio tasks own the sockets; pixelflux and pcmflux deliver frames on their
//! own threads into broadcast channels, which per-display fan-out tasks read, so no capture
//! callback ever touches a socket. Blocking native work (capture start and stop, the X
//! server, the sound server) runs on blocking threads or behind the `host` traits, and no
//! lock is held across an await: the state sits behind mutexes taken for short sections in the
//! order `state` gives, the sequencing locks (`reconfigure`, `video_capture`) are async.

pub mod audio;
pub mod client;
pub mod handler;
pub mod host;
pub mod link;
pub mod protocol;
pub mod relay;
pub mod server;
pub mod state;
pub mod steer;
pub mod uplink;
pub mod wiring;

#[cfg(test)]
mod policy_tests;
#[cfg(test)]
mod server_tests;
#[cfg(test)]
mod socket_tests;
#[cfg(test)]
pub(crate) mod testkit;

pub use handler::{PeerInfo, PeerSocket, routes, websocket_handler};
pub use host::WsDeps;
pub use server::{Timings, WsService};

use std::sync::OnceLock;
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static OFFSET: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

/// Where `now` starts: a stamp left at 0.0 means "never" throughout the plane, so every floor
/// measured from one has to read as long past, a page reconnecting the moment a server restarts
/// among them.
const CLOCK_ORIGIN_S: f64 = 86_400.0;

/// The monotonic clock the plane times everything by, in seconds.
pub fn now() -> f64 {
    let elapsed = CLOCK_ORIGIN_S + EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64();
    #[cfg(test)]
    let elapsed = elapsed + OFFSET.with(|o| o.get());
    elapsed
}

/// Unix time in seconds, for the session records.
pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
pub(crate) mod clock_tests {
    //! The suites move the plane's clock on without waiting: an offset per test thread, since
    //! `#[tokio::test]` runs each test and the tasks it spawns on one thread.

    use super::OFFSET;

    /// Move the clock `seconds` ahead.
    pub fn advance(seconds: f64) {
        OFFSET.with(|o| o.set(o.get() + seconds));
    }

    /// Start a test with the clock at real time.
    pub fn reset_clock() {
        OFFSET.with(|o| o.set(0.0));
    }
}
