/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The DPI verb and the frame-rate seed, over one page that changes display.
//!
//! `s,<dpi>` is handled on the WebSockets transport as it is on the data channel, and a frame
//! rate set through SETTINGS is the rate a display registering afterwards starts at.

use std::time::Duration;

use selkies_suites::{Checks, Client, server};
use serde_json::{Value, json};

/// A primary page's SETTINGS, with `over` applied on top.
fn settings(over: Value) -> String {
    let mut base = json!({
        "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": false, "framerate": 60, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    });
    if let (Some(base), Some(over)) = (base.as_object_mut(), over.as_object()) {
        base.extend(over.clone());
    }
    format!("SETTINGS,{base}")
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn the_dpi_verb_is_handled_and_a_new_display_starts_at_the_set_frame_rate() {
    let server = server::launch("parity").start().await;
    let mut checks = Checks::new("parity-check");
    let page = Client::connect(&server, "").await;
    page.send(&settings(json!({})));
    server
        .settle(0, "Stream settings active", Duration::from_secs(3))
        .await;

    // The 's,120' scaling opcode must be handled, not warned away.
    let mark = server.mark();
    page.send("s,120");
    let scaled = server.saw(mark, "DPI", Duration::from_millis(1500)).await;
    let said = server.log_from(mark);
    checks.check(
        "D3: 's,120' not dropped",
        !said.contains("unhandled on_scaling_ratio") || scaled,
        said.trim(),
    );

    let mark = server.mark();
    page.send("STOP_VIDEO");
    server
        .settle(mark, "Stopping all streams", Duration::from_secs(1))
        .await;
    let mark = server.mark();
    page.send("START_VIDEO");
    server
        .settle(mark, "Stream settings active", Duration::from_secs(1))
        .await;

    // SETTINGS framerate=33 seeds the session's rate, so a display registering afterwards
    // starts at 33 fps rather than the default 60.
    let mark = server.mark();
    page.send(&settings(json!({"framerate": 33, "video_bitrate": 2200})));
    server
        .settle(mark, "is now 33 fps", Duration::from_secs(2))
        .await;
    let mark = server.mark();
    page.send(&settings(
        json!({"framerate": 33, "video_bitrate": 2200, "displayId": "display2"}),
    ));
    server
        .settle(
            mark,
            "Capture started for 'display2'",
            Duration::from_secs(2),
        )
        .await;
    page.send("START_VIDEO");
    let seeded = server
        .saw(mark, "FPS: 33.0", Duration::from_millis(2500))
        .await
        || server.log_from(mark).contains("33.00 FPS");
    checks.check(
        "D4: framerate 33 seeded for new display",
        seeded,
        selkies_suites::tail(&server.log_from(mark), 4),
    );
    checks.finish(&server.stop().await);
}
