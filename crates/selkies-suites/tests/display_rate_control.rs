/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A display that registers after the primary switched to CRF starts in CRF.
//!
//! The switch reaches the server through SETTINGS or through the `_rc` verb; either way a second
//! display seeded afterwards must inherit it rather than the server's default, CBR, which the
//! mode its capture states shows.

use std::time::Duration;

use selkies_suites::{Checks, Client, server, tail};
use serde_json::{Value, json};

const STREAM_SETTINGS: &str = "Stream settings active";

/// The SETTINGS of the page of `display`, with `over` applied on top.
fn settings(display: &str, over: Value) -> String {
    let mut base = json!({
        "displayId": display, "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": false, "framerate": 60, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 4000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    });
    if let (Some(base), Some(over)) = (base.as_object_mut(), over.as_object()) {
        base.extend(over.clone());
    }
    format!("SETTINGS,{base}")
}

/// Switch the primary to CRF, through the `_rc` verb or through SETTINGS, then seed a second
/// display and check the mode its capture states.
async fn a_second_display_inherits_crf(through_the_verb: bool) {
    let label = if through_the_verb {
        "_rc-opcode"
    } else {
        "SETTINGS"
    };
    let server = server::launch(label)
        .env("SELKIES_ENABLE_RATE_CONTROL", "true")
        .start()
        .await;
    let mut checks = Checks::new(&format!("d4b-{label}"));
    let primary = Client::connect(&server, "").await;
    primary.send(&settings("primary", json!({})));
    server
        .settle(0, STREAM_SETTINGS, Duration::from_secs(4))
        .await;
    let mark = server.mark();
    if through_the_verb {
        primary.send("_rc,crf");
    } else {
        primary.send(&settings("primary", json!({"rate_control_mode": "crf"})));
    }
    // The switch restarts the primary's capture, which states its settings again.
    server
        .settle(mark, STREAM_SETTINGS, Duration::from_secs(4))
        .await;
    // The second display connects only after the rate control was set, so any CRF it shows must
    // have been inherited.
    let mark = server.mark();
    let second = Client::connect(&server, "").await;
    second.send(&settings("display2", json!({})));
    server
        .settle(mark, STREAM_SETTINGS, Duration::from_secs(3))
        .await;
    let since = server.log_from(mark);
    let stated = since
        .lines()
        .rfind(|line| line.contains(STREAM_SETTINGS))
        .map_or_else(|| tail(&since, 3), str::to_owned);
    checks.check(
        &format!("display2 inherits crf ({label})"),
        stated.contains("CRF"),
        &stated,
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_display_registering_after_a_settings_switch_to_crf_starts_in_crf() {
    a_second_display_inherits_crf(false).await;
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_display_registering_after_the_rc_verb_switched_to_crf_starts_in_crf() {
    a_second_display_inherits_crf(true).await;
}
