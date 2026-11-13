/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The stream's state machine under repetition.
//!
//! Stop and start cycles, encoder switches, and a reconnect inside the grace window must each
//! leave a capture running, and a settings toggle afterwards must be taken without complaint.

use std::time::Duration;

use selkies_suites::{Checks, Client, server, tail};
use serde_json::{Value, json};

const CAPTURE_STARTED: &str = "Capture started";
/// What a capture logs once its encoder is up: the state each step is taken from.
const STREAM_SETTINGS: &str = "Stream settings active";

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
async fn the_stream_survives_repeated_stops_encoder_switches_and_a_reconnect() {
    let server = server::launch("stress").start().await;
    let mut checks = Checks::new("stress");
    let page = Client::connect(&server, "").await;
    page.send(&settings(json!({})));
    server
        .settle(0, STREAM_SETTINGS, Duration::from_secs(5))
        .await;

    // Repeated stop/start cycles must each restart the capture.
    let mut stuck = None;
    for cycle in 0..10 {
        let mark = server.mark();
        page.send("STOP_VIDEO");
        server
            .settle(mark, "Stopping all streams", Duration::from_secs(1))
            .await;
        page.send("START_VIDEO");
        if !server
            .saw(mark, CAPTURE_STARTED, Duration::from_secs(9))
            .await
        {
            stuck = Some((cycle, tail(&server.log_from(mark), 4)));
            break;
        }
        server
            .settle(mark, STREAM_SETTINGS, Duration::from_millis(1200))
            .await;
    }
    match stuck {
        Some((cycle, said)) => {
            checks.check(&format!("cycle {cycle}: capture restarted"), false, said);
        }
        None => checks.check("10 stop/start cycles all restart", true, ""),
    }

    // Encoder toggles must each bring the capture back up.
    for encoder in ["jpeg", "h264enc", "vp8enc", "h264enc"] {
        let mark = server.mark();
        page.send(&settings(json!({"encoder": encoder})));
        checks.check(
            &format!("encoder switch to {encoder}"),
            server
                .saw(mark, CAPTURE_STARTED, Duration::from_secs(12))
                .await,
            tail(&server.log_from(mark), 3),
        );
        server
            .settle(mark, STREAM_SETTINGS, Duration::from_secs(1))
            .await;
    }

    // Disconnect and reconnect within the grace window; the fresh page's SETTINGS must still
    // bring the capture up cleanly.
    let mark = server.mark();
    page.send("STOP_VIDEO");
    server
        .settle(mark, "Stopping all streams", Duration::from_millis(800))
        .await;
    page.close().await;
    let page = Client::connect(&server, "").await;
    page.send(&settings(json!({})));
    checks.check(
        "reconnect captures cleanly",
        server
            .saw(mark, CAPTURE_STARTED, Duration::from_secs(14))
            .await,
        "",
    );
    server
        .settle(mark, STREAM_SETTINGS, Duration::from_secs(4))
        .await;
    // A binary-clipboard toggle must be accepted without complaint: nothing names the setting.
    let mark = server.mark();
    page.send(&settings(json!({"enable_binary_clipboard": false})));
    checks.check(
        "binary clipboard toggle accepted",
        server
            .stays_without(mark, "enable_binary_clipboard", Duration::from_millis(1500))
            .await,
        tail(&server.log_from(mark), 3),
    );
    checks.finish(&server.stop().await);
}
