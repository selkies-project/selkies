/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A primary that shrinks beside a secondary keeps the second screen (Wayland).
//!
//! Each display is a screen of the session compositor's own, so the secondary's output has to
//! move into the room the primary gives up, and the compositor refuses an output rectangle that
//! overlaps a live one: the layout pass must therefore shrink the primary's screen before it
//! recreates the secondary at its new offset, or the second screen is dropped on every primary
//! shrink. Driven with raw WebSocket clients on the websockets transport against the in-process
//! pixelflux compositor, no browser.

use std::time::Duration;

use selkies_suites::displays::layouts_in;
use selkies_suites::{Checks, Client, server};
use serde_json::json;

fn settings(display_id: &str, width: u32, height: u32) -> String {
    format!(
        "SETTINGS,{}",
        json!({
            "displayId": display_id, "initialClientWidth": width, "initialClientHeight": height,
            "manual_resolution": false, "framerate": 30, "encoder": "jpeg",
            "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128_000,
            "scaling_dpi": 96, "displayPosition": "right",
        })
    )
}

#[tokio::test]
#[ignore = "needs a built selkies server"]
async fn wayland_primary_shrink() {
    let server = server::launch("shrink").wayland(true).start().await;
    let mut checks = Checks::new("wl-primary-shrink");
    let primary = Client::connect(&server, "").await;
    let mark = server.mark();
    primary.send(&settings("primary", 1920, 1080));
    checks.check(
        "primary capture starts",
        server
            .saw(
                mark,
                "Capture started for 'primary'",
                Duration::from_secs(45),
            )
            .await,
        "",
    );
    tokio::time::sleep(Duration::from_secs(1)).await;

    let secondary = Client::connect(&server, "").await;
    let mark = server.mark();
    secondary.send(&settings("display2", 1280, 720));
    checks.check(
        "secondary capture starts beside the primary",
        server
            .saw(
                mark,
                "Capture started for 'display2'",
                Duration::from_secs(45),
            )
            .await,
        "",
    );
    let before = layouts_in(&server.log_from(mark)).pop().unwrap_or_default();
    checks.check(
        "secondary laid out at the primary's right edge",
        before.get("display2").is_some_and(|r| r.x == 1920),
        format!("{before:?}"),
    );
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mark = server.mark();
    let texts = secondary.texts();
    primary.send("r,1280x720,primary");
    checks.check(
        "the shrink recreates the secondary output at its new offset",
        server
            .saw(
                mark,
                "Wayland output 2 moves to +1280+0; recreating it.",
                Duration::from_secs(30),
            )
            .await,
        "",
    );
    checks.check(
        "secondary capture comes back after the shrink",
        server
            .saw(
                mark,
                "Capture started for 'display2'",
                Duration::from_secs(45),
            )
            .await,
        "",
    );
    let killed = secondary
        .texts_during(texts, Duration::from_secs(3), |m| m.starts_with("KILL"))
        .await;
    let tail = server.log_from(mark);
    checks.check(
        "no output creation was refused",
        !tail.contains("CreateOutput 2: rejected") && !tail.contains("cannot create an output"),
        "",
    );
    checks.check(
        "the secondary was not dropped",
        !tail.contains("' dropped: ") && killed.is_empty() && !secondary.closed(),
        format!("{:?}", &killed[..killed.len().min(3)]),
    );
    let after = layouts_in(&tail).pop().unwrap_or_default();
    checks.check(
        "the new layout keeps both displays, secondary at the shrunken edge",
        after.get("primary").is_some_and(|r| r.w == 1280)
            && after.get("display2").is_some_and(|r| r.x == 1280),
        format!("{after:?}"),
    );
    primary.send("STOP_VIDEO");
    secondary.send("STOP_VIDEO");
    tokio::time::sleep(Duration::from_millis(500)).await;
    secondary.close().await;
    primary.close().await;
    let log = server.stop().await;
    checks.finish(&log);
}
