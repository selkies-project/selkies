/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `/api/switch` flipped between the transports six times under an attached page.
//!
//! Each flip has to land on the mode it names with the HTTP routes answering. The desktop the
//! transports share is checked too: logical monitors belong to whichever layout engine defined
//! them, and the flip happens under a page that is still attached, so nothing tears the layout
//! down on the way out; one left behind has window managers tiling panels and maximized windows
//! against a rectangle that is no longer the screen. That attached page is also what makes the
//! capture teardown worth checking here: its display entry outlives the switch by the reconnect
//! grace, so the reconfigure pass takes its clients-present branch, and a shutdown that leaned
//! on it would leave the capture encoding for nobody.

use std::process::Command;
use std::time::Duration;

use selkies_suites::server::{output_within, test_display};
use selkies_suites::x11::has_pluggable_outputs;
use selkies_suites::{Checks, Client, Server, http, server};
use serde_json::{Value, json};

/// The log between each WebSockets transport shutdown's first and last line.
fn shutdown_windows(log: &str) -> Vec<&str> {
    let mut windows = Vec::new();
    let mut rest = log;
    while let Some(start) = rest.find("Transport shutdown initiated") {
        let Some(length) = rest[start..].find("Transport shutdown complete") else {
            break;
        };
        windows.push(&rest[start..start + length]);
        rest = &rest[start + length..];
    }
    windows
}

/// Monitors the layout engine put on the test display beyond the server's own screen: the
/// `selkies-*` logical monitors, or the secondary outputs it plugged in where displays are
/// `outputs`.
fn layout_monitors(outputs: bool) -> Vec<String> {
    let listing = output_within(
        Command::new("xrandr")
            .arg("--listmonitors")
            .env("DISPLAY", test_display()),
        Duration::from_secs(20),
    )
    .unwrap_or_default();
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let index = fields.next()?.trim_end_matches(':');
            let name = fields.next()?;
            fields.next()?;
            (!index.is_empty() && index.bytes().all(|b| b.is_ascii_digit())).then_some(name)
        })
        .filter(|name| name.contains("selkies-") || (outputs && !name.contains('*')))
        .map(|name| name.trim_start_matches(['+', '*']).to_owned())
        .collect()
}

/// The mode `/api/status` reports.
async fn current_mode(server: &Server) -> String {
    http::get(server.port(), "/api/status")
        .await
        .ok()
        .and_then(|response| serde_json::from_slice::<Value>(&response.body).ok())
        .and_then(|status| status["current_mode"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Flip the transport six times under a page of `wayland` or the X test display. The monitor
/// checks are X11's; the teardown checks are not.
async fn flip_under_a_page(wayland: bool) {
    let label = if wayland { "switch-wayland" } else { "switch" };
    let server = server::launch(label)
        .wayland(wayland)
        .env("SELKIES_DEBUG", "true")
        .start()
        .await;
    let mut checks = Checks::new(label);
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
    server
        .settle(0, "Stream settings active", Duration::from_secs(5))
        .await;
    let mut outputs = false;
    if !wayland {
        outputs = has_pluggable_outputs(&test_display());
        if outputs {
            checks.skip(
                "the layout engine defined this client's monitor",
                "displays are outputs; one display defines no monitor",
            );
        } else {
            let monitors = layout_monitors(false);
            checks.check(
                "the layout engine defined this client's monitor",
                !monitors.is_empty(),
                format!("{monitors:?}"),
            );
        }
    }

    for (flip, target) in ["webrtc", "websockets"]
        .into_iter()
        .cycle()
        .take(6)
        .enumerate()
    {
        let mark = server.mark();
        let answer = http::request(
            server.port(),
            "POST",
            "/api/switch",
            &[("Content-Type", "application/json")],
            json!({"mode": target}).to_string().as_bytes(),
            Duration::from_secs(10),
        )
        .await;
        let switched = matches!(answer, Ok(ref response) if response.status == 200);
        checks.check(
            &format!("switch {flip}->{target} returns 200"),
            switched,
            answer.map_or_else(|why| why, |response| response.text()),
        );
        if switched {
            let mode = current_mode(&server).await;
            checks.check(
                &format!("switch {flip}: mode={target}"),
                mode == target,
                &mode,
            );
        }
        server
            .settle(
                mark,
                &format!("Starting service: {target}"),
                Duration::from_millis(1500),
            )
            .await;
        if target == "webrtc" && !wayland {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
            let mut left = layout_monitors(outputs);
            while !left.is_empty() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
                left = layout_monitors(outputs);
            }
            checks.check(
                &format!("switch {flip}: no monitor of the stopped transport is left"),
                left.is_empty(),
                format!("{left:?}"),
            );
        }
    }

    // A shutdown whose reconfigure pass still saw display clients (the reconnect grace holds
    // them well past the switch) is one where that pass stopped nothing, so the capture is
    // shutdown's own to stop.
    let log = server.log();
    let windows = shutdown_windows(&log);
    let held: Vec<&&str> = windows
        .iter()
        .filter(|window| window.contains("Calculating new extended desktop layout"))
        .collect();
    checks.check(
        "a shutdown that outran the reconnect grace stopped its own capture",
        !held.is_empty()
            && held
                .iter()
                .all(|window| window.contains("Stopping all streams")),
        format!(
            "{} of {} shutdown(s) held clients",
            held.len(),
            windows.len()
        ),
    );
    checks.check(
        "no cursor callback outlived its server",
        !log.contains("Error handling pixelflux cursor"),
        "",
    );
    let mode = current_mode(&server).await;
    checks.check("final mode is websockets", mode == "websockets", &mode);
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server, the X test display (E2E_DISPLAY), and xrandr"]
async fn the_transport_flips_cleanly_under_an_attached_page_on_x11() {
    flip_under_a_page(false).await;
}

#[tokio::test]
#[ignore = "needs a built selkies server with the Wayland backend"]
async fn the_transport_flips_cleanly_under_an_attached_page_on_wayland() {
    flip_under_a_page(true).await;
}
