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
//!
//! Third-party crates (smithay, the `rtc_*` stacks) log under their module paths, which is how
//! they are told apart: they stay at WARNING in every mode, and those warnings are paced by the
//! same filter, since `rtc_dtls` warns per unknown extension in every handshake with Chromium.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Targets whose DEBUG records are paced by template.
const PACED_PREFIXES: &[&str] = &["webrtc.", "ice."];
/// Targets that fire per packet, held at INFO even in debug mode.
const PACKET_TARGETS: &[&str] = &["webrtc.rtp.sender", "webrtc.rtp.receiver", "webrtc.sctp"];
/// The short names Selkies logs under; anything else is a third-party crate's.
const OWN_TARGETS: &[&str] = &[
    "main",
    "server",
    "ws",
    "webrtc",
    "signaling",
    "display",
    "input",
    "gamepad",
    "audio",
    "printing",
    "webcam",
    "stats",
    "audit",
    "settings",
    "metrics",
    "capture_demand",
    "access",
    "ice",
];

/// Whether a record's target is Selkies' own rather than a third-party crate's.
fn is_own(target: &str) -> bool {
    OWN_TARGETS.contains(&target)
        || target.starts_with("webrtc.")
        || target.starts_with("ice.")
        || target == "selkies"
        || target.starts_with("selkies::")
        || target.starts_with("selkies_rtc")
}

/// Message texts the pacing remembers before it starts over.
const PACE_TEMPLATES: usize = 4096;
/// How often one template of a paced target gets through at DEBUG.
const PACE_PERIOD: Duration = Duration::from_secs(5);

/// Rate-limit DEBUG records from the chatty stacks by template (`PACED_PREFIXES`).
struct PacedFilter {
    seen: Mutex<HashMap<(String, String), (Instant, u64)>>,
}

impl PacedFilter {
    /// Whether the record passes, and the count of alike records it stands for.
    fn admit(&self, target: &str, template: &str) -> Option<u64> {
        let mut seen = self.seen.lock().unwrap();
        let now = Instant::now();
        // The text is formatted, so a peer's bytes in a message would otherwise grow this forever.
        if seen.len() >= PACE_TEMPLATES {
            seen.clear();
        }
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
    debug: AtomicBool,
    pace: PacedFilter,
}

/// The process's logger, installed before the settings are parsed so the warnings they raise
/// reach the log, and told the debug flag once they are.
static LOGGER: LazyLock<Logger> = LazyLock::new(|| Logger {
    debug: AtomicBool::new(false),
    pace: PacedFilter {
        seen: Mutex::new(HashMap::new()),
    },
});

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let target = metadata.target();
        let level = metadata.level();
        if !is_own(target) {
            return level <= Level::Warn;
        }
        if PACKET_TARGETS.contains(&target) {
            return level <= Level::Info;
        }
        if self.debug.load(Ordering::Relaxed) {
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
        let target = record.target();
        let paced = !is_own(target)
            || (record.level() == Level::Debug
                && PACED_PREFIXES.iter().any(|p| target.starts_with(p)));
        if paced {
            match self.pace.admit(target, &message) {
                None => return,
                Some(0) => {}
                Some(held) => {
                    suffix = format!(" (+{held} alike in the last {}s)", PACE_PERIOD.as_secs());
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

/// Install the logger for this process at the level policy `debug` names; called again, as
/// once the settings that hold the flag are parsed, it moves the policy.
pub fn configure(debug: bool) {
    let _ = log::set_logger(&*LOGGER);
    LOGGER.debug.store(debug, Ordering::Relaxed);
    log::set_max_level(if debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logger(debug: bool) -> Logger {
        Logger {
            debug: AtomicBool::new(debug),
            pace: PacedFilter {
                seen: Mutex::new(HashMap::new()),
            },
        }
    }

    fn enabled(l: &Logger, target: &str, level: Level) -> bool {
        l.enabled(&Metadata::builder().target(target).level(level).build())
    }

    #[test]
    fn a_third_party_crate_stays_at_warning_even_in_debug_mode() {
        for debug in [false, true] {
            let l = logger(debug);
            assert!(!enabled(&l, "smithay::wayland::output", Level::Info));
            assert!(!enabled(&l, "rtc_dtls::handshake", Level::Debug));
            assert!(enabled(&l, "rtc_dtls::handshake", Level::Warn));
        }
    }

    #[test]
    fn selkies_targets_follow_the_debug_switch() {
        assert!(enabled(&logger(false), "ws", Level::Info));
        assert!(!enabled(&logger(false), "ws", Level::Debug));
        assert!(enabled(&logger(true), "ws", Level::Debug));
        assert!(enabled(&logger(true), "ice.agent", Level::Debug));
        assert!(enabled(
            &logger(true),
            "selkies_rtc::rtp::pacer",
            Level::Debug
        ));
        assert!(!enabled(&logger(true), "webrtc.sctp", Level::Debug));
    }

    #[test]
    fn a_repeated_template_is_held_back_within_its_period() {
        let l = logger(false);
        assert_eq!(l.pace.admit("rtc_dtls::x", "unknown extension"), Some(0));
        assert_eq!(l.pace.admit("rtc_dtls::x", "unknown extension"), None);
        assert_eq!(l.pace.admit("rtc_dtls::x", "another line"), Some(0));
        for i in 0..(PACE_TEMPLATES * 2) {
            l.pace.admit("rtc_dtls::x", &format!("peer value {i}"));
        }
        assert!(l.pace.seen.lock().unwrap().len() <= PACE_TEMPLATES);
    }
}
