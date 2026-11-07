/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Per-frame send stamps for the comparison benchmark, only in a `frame-log` build.
//!
//! One line per frame handed to the controller,
//! `kind now_ns capture_ns encode_start_ns encode_end_ns bytes rtp_packets keyframe`, stamped at
//! two points: `ws` when a frame's bytes go to the socket and `wr` when its last RTP
//! packet leaves the pacer. Lines buffer in memory and a thread flushes them once a second, so
//! the stamp costs the send path a lock and a push rather than a file write.

use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// One frame's stamps.
pub struct FrameRecord {
    pub kind: &'static str,
    pub capture_ns: i64,
    pub encode_start_ns: i64,
    pub encode_end_ns: i64,
    pub bytes: usize,
    pub rtp_packets: u32,
    pub keyframe: bool,
}

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static FLUSHER: OnceLock<bool> = OnceLock::new();

/// Record a frame sent now; nothing without `BASELINE_FRAME_LOG` set.
pub fn record(frame: FrameRecord) {
    let enabled = FLUSHER.get_or_init(|| {
        let Some(path) = std::env::var_os("BASELINE_FRAME_LOG") else {
            return false;
        };
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let batch = std::mem::take(&mut *LINES.lock().unwrap_or_else(|e| e.into_inner()));
                if batch.is_empty() {
                    continue;
                }
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = file.write_all(batch.concat().as_bytes());
                }
            }
        });
        true
    });
    if !*enabled {
        return;
    }
    let line = format!(
        "{} {} {} {} {} {} {} {}\n",
        frame.kind,
        super::now_ns(),
        frame.capture_ns,
        frame.encode_start_ns,
        frame.encode_end_ns,
        frame.bytes,
        frame.rtp_packets,
        u8::from(frame.keyframe),
    );
    LINES.lock().unwrap_or_else(|e| e.into_inner()).push(line);
}
