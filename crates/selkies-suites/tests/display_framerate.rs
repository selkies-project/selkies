/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A display's frame rate: inherited when it opens, followed live by the X output afterwards.
//!
//! A primary sets its rate through SETTINGS, then a second display opens without one (a browser
//! with no stored per-display value): its capture must inherit 33 fps, not the server's default
//! 60. Then the primary's rate changes live, through SETTINGS and the `_arg_fps` verb: the X
//! server's output must follow to a mode at the new rate, read from `xrandr`, without a capture
//! restarting. A display refreshing at an NTSC rate is asked for as the fraction it names
//! (60000/1001), which both paths carry whole: a display opening at it states 59.94 fps, and the
//! verb applies it rather than refusing a rate that is no whole number.

use std::process::Command;
use std::time::Duration;

use selkies_suites::server::{output_within, test_display};
use selkies_suites::{Checks, Client, server};
use serde_json::{Value, json};

const STREAM_SETTINGS: &str = "Stream settings active";
/// How long a live rate change is watched: for the output to follow, and for no capture to start.
const LIVE: Duration = Duration::from_secs(3);

/// The SETTINGS of the page of `display`, which names no frame rate unless `over` does.
fn settings(display: &str, over: Value) -> Value {
    let mut base = json!({
        "displayId": display, "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": false, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    });
    if let (Some(base), Some(over)) = (base.as_object_mut(), over.as_object()) {
        base.extend(over.clone());
    }
    base
}

/// The capture's "Stream settings active" lines in `text`, which carry the settings a capture
/// actually started with.
fn stated(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.contains(STREAM_SETTINGS))
        .collect()
}

/// The refresh of the mode the test X server's output shows, 0 when it shows none.
fn output_refresh() -> f64 {
    let listing = output_within(
        Command::new("xrandr").env("DISPLAY", test_display()),
        Duration::from_secs(10),
    )
    .unwrap_or_default();
    // The current mode's rate is the number an asterisk follows.
    listing
        .match_indices('*')
        .find_map(|(at, _)| {
            let digits = listing[..at]
                .rfind(|c: char| !c.is_ascii_digit() && c != '.')
                .map_or(0, |before| before + 1);
            let rate = &listing[digits..at];
            rate.contains('.').then(|| rate.parse().ok()).flatten()
        })
        .unwrap_or(0.0)
}

#[tokio::test]
#[ignore = "needs a built selkies server, the X test display (E2E_DISPLAY), and xrandr"]
async fn a_frame_rate_is_inherited_by_new_displays_and_followed_live_by_the_output() {
    let server = server::launch("framerate").start().await;
    let mut checks = Checks::new("d4");
    let primary = Client::connect(&server, "").await;
    primary.send(&format!(
        "SETTINGS,{}",
        settings("primary", json!({"framerate": 33, "video_bitrate": 2200}))
    ));
    server
        .settle(0, STREAM_SETTINGS, Duration::from_secs(4))
        .await;

    // SETTINGS without a frame rate or a bitrate (a fresh browser, no per-display stored value):
    // its capture must inherit the primary's live 33 fps and 2200 kbps.
    let mark = server.mark();
    let second = Client::connect(&server, "").await;
    let mut bare = settings("display2", json!({}));
    if let Some(bare) = bare.as_object_mut() {
        bare.remove("video_bitrate");
    }
    second.send(&format!("SETTINGS,{bare}"));
    server
        .settle(mark, STREAM_SETTINGS, Duration::from_secs(4))
        .await;
    let before = server.log_from(0);
    let (before, since) = before.split_at(mark.min(before.len()));
    let second_stated = stated(since);
    let first = second_stated.first().copied().unwrap_or_default();
    checks.check(
        "display2 capture carries client framerate",
        second_stated.iter().any(|line| line.contains("FPS: 33")),
        first,
    );
    // The primary set its bitrate explicitly, so the primary's own line says whether this capture
    // reports one at all; where it does not, inheritance is unobservable here.
    if stated(before).iter().any(|line| line.contains("2200")) {
        checks.check(
            "display2 capture carries client bitrate",
            second_stated.iter().any(|line| line.contains("2200")),
            first,
        );
    } else {
        checks.skip(
            "display2 capture carries client bitrate",
            "the capture omits bitrate from its status line",
        );
    }
    // The rate changes are made on the primary alone: the second display's page leaves, and the
    // grace a returning page is given has to pass before its display does.
    let mark = server.mark();
    second.close().await;
    server
        .settle(
            mark,
            "Client for 'display2' did not return within the grace period",
            Duration::from_secs(6),
        )
        .await;
    server
        .settle(mark, "followed the new layout live", Duration::from_secs(2))
        .await;

    for (fps, verb) in [
        (
            144.0,
            format!(
                "SETTINGS,{}",
                settings("primary", json!({"framerate": 144}))
            ),
        ),
        (90.0, "_arg_fps,90".to_owned()),
    ] {
        let mark = server.mark();
        primary.send(&verb);
        let quiet = server.stays_without(mark, "Capture started for", LIVE);
        let follows = async {
            let deadline = tokio::time::Instant::now() + LIVE;
            loop {
                let rate = output_refresh();
                if (rate - fps).abs() <= fps * 0.01 || tokio::time::Instant::now() >= deadline {
                    return rate;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let (quiet, rate) = tokio::join!(quiet, follows);
        checks.check(
            &format!("output refresh follows a live {fps} fps"),
            (rate - fps).abs() <= fps * 0.01,
            format!("{rate:.2} Hz"),
        );
        checks.check(&format!("no capture restarts for {fps} fps"), quiet, "");
    }

    let ntsc = 60000.0 / 1001.0;
    let mark = server.mark();
    primary.send(&format!("_arg_fps,{ntsc}"));
    checks.check(
        "the verb applies an NTSC rate whole",
        server
            .saw(
                mark,
                "Applied framerate live via '_arg_fps': 59.94 fps",
                Duration::from_secs(12),
            )
            .await,
        selkies_suites::tail(&server.log_from(mark), 2),
    );
    let mark = server.mark();
    let third = Client::connect(&server, "").await;
    third.send(&format!(
        "SETTINGS,{}",
        settings("display2", json!({"framerate": ntsc}))
    ));
    checks.check(
        "a display opening at an NTSC rate states it whole",
        server
            .saw(mark, "59.94 fps.", Duration::from_secs(12))
            .await,
        selkies_suites::tail(&server.log_from(mark), 2),
    );
    checks.finish(&server.stop().await);
}
