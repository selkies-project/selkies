/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A display that registers after the primary set its frame rate starts at that rate.
//!
//! Each display is a page of its own, so the second display's SETTINGS arrive on a fresh socket
//! once the primary's are in force, and its capture has to state the client's 33 fps rather than
//! the server's default 60.

use std::time::Duration;

use selkies_suites::{Checks, Client, server, tail};
use serde_json::json;

/// The SETTINGS of the page of `display` at 33 fps.
fn settings(display: &str) -> String {
    format!(
        "SETTINGS,{}",
        json!({
            "displayId": display, "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": false, "framerate": 33, "encoder": "h264enc",
            "video_crf": 25, "video_bitrate": 2200, "audio_bitrate": 128000,
            "scaling_dpi": 96, "displayPosition": "right",
        })
    )
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_display_registering_later_starts_at_the_clients_frame_rate() {
    let server = server::launch("seed").start().await;
    let mut checks = Checks::new("D4-probe");
    let primary = Client::connect(&server, "").await;
    primary.send(&settings("primary"));
    // The primary's own capture states the rate too; the mark has to fall after it.
    server
        .settle(0, "Stream settings active", Duration::from_secs(4))
        .await;
    let mark = server.mark();
    let second = Client::connect(&server, "").await;
    second.send(&settings("display2"));
    checks.check(
        "D4: display2 seeded at client framerate",
        server.saw(mark, "FPS: 33", Duration::from_secs(3)).await,
        tail(&server.log_from(mark), 4),
    );
    checks.finish(&server.stop().await);
}
