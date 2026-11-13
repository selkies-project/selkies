/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The latency the server reports is the path's, not the client's ack cadence.
//!
//! Frame acks carry how long the client held the id before its ack tick fired. A backgrounded
//! tab clamps that tick to a second, so without the hold the server would read a second of the
//! client's own timer as network latency and report it for as long as the smoothing window holds
//! those samples. Held acks are replayed here with and without the hold, over the real socket,
//! and the reported figure has to follow the path either way. The server runs Turbo, so frames
//! keep coming off the still test display and every round has a fresh id to hold.

use std::time::{Duration, Instant};

use selkies_suites::{Checks, Client, server};
use serde_json::{Value, json};

/// One round per smoothing sample, so the window is entirely held acks.
const ROUNDS: usize = 22;
const HOLD: Duration = Duration::from_millis(300);
/// Half the hold, in milliseconds: a figure under it follows the path, one over it the hold.
const HALF_HOLD_MS: f64 = 150.0;

/// A page acking the frames it is sent.
struct Acker<'a> {
    page: &'a Client,
    acked: Option<u16>,
}

impl Acker<'_> {
    /// Ack `frame` unless it was the last one acked, reporting how long it waited when asked to.
    fn ack(&mut self, frame: Option<(u16, Instant)>, report_hold: bool) {
        let Some((id, arrived)) = frame else { return };
        if self.acked == Some(id) {
            return;
        }
        self.acked = Some(id);
        self.page.send(&if report_hold {
            format!("CLIENT_FRAME_ACK {id} {}", arrived.elapsed().as_millis())
        } else {
            format!("CLIENT_FRAME_ACK {id}")
        });
    }

    /// Fill the smoothing window with acks held [`HOLD`] past their frame; the latency the
    /// server reports afterwards. The id acked is the one that was newest before the hold, so it
    /// really is that old when its ack goes out: a client whose ack tick was clamped.
    async fn held_acks(&mut self, report_hold: bool) -> f64 {
        for _ in 0..ROUNDS {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let frame = self.page.newest_frame();
            tokio::time::sleep(HOLD).await;
            self.ack(frame, report_hold);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        reported(self.page)
    }
}

/// The round trip the server last reported to `page`, -1 when it reported none.
fn reported(page: &Client) -> f64 {
    page.last_text(|text| {
        text.contains("\"stream_stats\"")
            && serde_json::from_str::<Value>(text)
                .is_ok_and(|stats| stats["stats"]["rtt_ms"].is_number())
    })
    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    .and_then(|stats| stats["stats"]["rtt_ms"].as_f64())
    .unwrap_or(-1.0)
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn the_reported_latency_follows_the_path_whatever_the_ack_cadence() {
    let server = server::launch("latency")
        .env("SELKIES_USE_CPU", "true")
        .env("SELKIES_VIDEO_STREAMING_MODE", "true")
        .start()
        .await;
    let mut checks = Checks::new("ack-latency");
    let page = Client::connect(&server, "").await;
    page.send(&format!(
        "SETTINGS,{}",
        json!({
            "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": false, "framerate": 60, "encoder": "h264enc",
            "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
            "scaling_dpi": 96, "displayPosition": "right",
        })
    ));
    // The round trip is reported only to a page with its stats open.
    page.send("_stats,1");
    let mut acker = Acker {
        page: &page,
        acked: None,
    };
    // Prompt acks first, so the window starts on real round trips.
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(50)).await;
        acker.ack(page.newest_frame(), true);
    }
    let prompt = reported(&page);
    checks.check(
        "prompt acks report the path",
        (0.0..HALF_HOLD_MS).contains(&prompt),
        prompt,
    );

    let held = acker.held_acks(true).await;
    checks.check(
        "a held ack that reports its hold still reports the path",
        (0.0..HALF_HOLD_MS).contains(&held),
        held,
    );

    // The control: the same holds, unreported, do move the figure, so the check above is
    // measuring the subtraction and not a quiet link.
    let silent = acker.held_acks(false).await;
    checks.check(
        "an ack that hides its hold is taken at face value",
        silent > HALF_HOLD_MS,
        silent,
    );
    checks.finish(&server.stop().await);
}
