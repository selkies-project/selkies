/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The one place the server's logging is configured.
//!
//! Every module logs through a short target that says which part of the server spoke
//! (`main`, `server`, `ws`, `webrtc`, `signaling`, `display`, `input`, `gamepad`, `audio`,
//! `printing`, `webcam`, `stats`, `audit`, `settings`), rendered as `LEVEL:name:message`
//! on stderr like pixelflux's own bracketed `[X11]`/`[Wayland]` lines beside them. INFO is
//! the story of a session as an operator reads it back from a user: the settings the
//! server came up with, what a client connected as, the capture and encoder path each
//! display took, and every later change or failure; the mechanics behind those events
//! (each reconfiguration step, each broadcast, each task) are DEBUG. Debug mode
//! (`--debug`) opens DEBUG on every target, and the same flag reaches pixelflux as
//! `debug_logging`.
//!
//! The WebRTC and ICE stacks would bury a debug run on their own: `PacedFilter` lets the
//! first record of each message template under a paced target through and then one per
//! `PACE_PERIOD` seconds, carrying the count of the records it held back, while the
//! per-packet RTP and SCTP targets stay at INFO outright.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Targets whose DEBUG records are paced by template.
const PACED_PREFIXES: &[&str] = &["webrtc.", "ice."];
/// Targets that fire per packet, held at INFO even in debug mode.
const PACKET_TARGETS: &[&str] = &["webrtc.rtp.sender", "webrtc.rtp.receiver", "webrtc.sctp"];
/// How often one template of a paced target gets through at DEBUG.
const PACE_PERIOD: Duration = Duration::from_secs(5);

/// Rate-limit DEBUG records from the chatty stacks by template (`PACED_PREFIXES`).
struct PacedFilter {
    seen: Mutex<HashMap<(String, String), (Instant, u64)>>,
}

impl PacedFilter {
    /// Whether the record passes, and the count of alike records it stands for.
    fn admit(&self, target: &str, template: &str) -> Option<u64> {
        if !PACED_PREFIXES.iter().any(|p| target.starts_with(p)) {
            return Some(0);
        }
        let mut seen = self.seen.lock().unwrap();
        let now = Instant::now();
        let key = (target.to_string(), template.to_string());
        match seen.get_mut(&key) {
            Some((last, held)) if now.duration_since(*last) < PACE_PERIOD => {
                *held += 1;
                None
            }
            Some((last, held)) => {
                let count = *held;
                *last = now;
                *held = 0;
                Some(count)
            }
            None => {
                seen.insert(key, (now, 0));
                Some(0)
            }
        }
    }
}

/// The server's logger: `LEVEL:target:message` lines on stderr.
struct Logger {
    debug: bool,
    pace: PacedFilter,
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let target = metadata.target();
        let level = metadata.level();
        if PACKET_TARGETS.contains(&target) {
            return level <= Level::Info;
        }
        if self.debug {
            return true;
        }
        level <= Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        let mut suffix = String::new();
        if record.level() == Level::Debug {
            match self.pace.admit(record.target(), &message) {
                None => return,
                Some(0) => {}
                Some(held) => {
                    suffix = format!(
                        " (+{held} alike in the last {}s)",
                        PACE_PERIOD.as_secs()
                    );
                }
            }
        }
        let level = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARNING",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        let stderr = std::io::stderr();
        let mut out = stderr.lock();
        let _ = writeln!(out, "{level}:{}:{message}{suffix}", record.target());
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Install the logger and the level policy for this process; a second call is ignored.
pub fn configure(debug: bool) {
    let logger = Box::new(Logger {
        debug,
        pace: PacedFilter {
            seen: Mutex::new(HashMap::new()),
        },
    });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(if debug {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        });
    }
}
