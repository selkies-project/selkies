/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a page sends before its initial SETTINGS must not hold that SETTINGS.
//!
//! A page sends its first SETTINGS once its decoder probes answer, and a resize, a DPI sync, or
//! the cursor-rendering choice can reach the server ahead of it. The WebSockets receive loop reads
//! a connection's messages in order, so none of them may wait there for the layout that the
//! SETTINGS read behind them makes. The resize and the DPI travel in that SETTINGS as well; the
//! cursor-rendering choice does not, and is still taken.

use std::time::{Duration, Instant};

use selkies_suites::{Checks, Client, server};
use serde_json::json;

/// Logged once the receive loop has read the SETTINGS, before it applies them.
const READ: &str = "Parsed client settings";
const APPLIED: &str = "settings applied for 'primary'";
const CURSOR_TAKEN: &str = "Received SET_NATIVE_CURSOR_RENDERING: True";
/// Verbs a page can send ahead of its SETTINGS, and what shows each was taken where it must be.
const EARLY: [(&str, &str, Option<&str>); 3] = [
    ("a resize", "r,1600x900,primary", None),
    ("a DPI sync", "s,120", None),
    (
        "the cursor-rendering choice",
        "SET_NATIVE_CURSOR_RENDERING,1",
        Some(CURSOR_TAKEN),
    ),
];

/// The initial SETTINGS a primary page sends.
fn settings() -> String {
    format!(
        "SETTINGS,{}",
        json!({
            "displayId": "primary",
            "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": false, "framerate": 60, "encoder": "h264enc",
            "scaling_dpi": 96,
        })
    )
}

/// Send each early verb, then the initial SETTINGS, to a server of its own: only a session's first
/// SETTINGS is ever waited for.
async fn early_verbs_do_not_hold_the_settings(wayland: bool) {
    let backend = if wayland { "wl" } else { "x11" };
    let mut checks = Checks::new(&format!("verbs-before-settings-{backend}"));
    let mut logs = String::new();
    for (label, verb, taken) in EARLY {
        let server = server::launch(backend)
            .wayland(wayland)
            .env("SELKIES_DEBUG", "true")
            .start()
            .await;
        let page = Client::connect(&server, "").await;
        page.send(verb);
        let sent = Instant::now();
        page.send(&settings());
        let read = server.saw_at(0, READ, Duration::from_secs(40)).await;
        checks.check(
            &format!("{label} ahead of the initial SETTINGS does not hold it"),
            read.is_some_and(|at| at - sent < Duration::from_secs(5)),
            read.map_or_else(
                || "never read".to_owned(),
                |at| format!("read {:.1}s after it was sent", (at - sent).as_secs_f32()),
            ),
        );
        checks.check(
            &format!("the SETTINGS behind {label} are applied"),
            server.saw(0, APPLIED, Duration::from_secs(40)).await,
            "",
        );
        if let Some(taken) = taken {
            checks.check(
                &format!("{label} ahead of the initial SETTINGS is still taken"),
                server.saw(0, taken, Duration::from_secs(10)).await,
                "",
            );
        }
        logs.push_str(&server.stop().await);
    }
    checks.finish(&logs);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn verbs_ahead_of_the_initial_settings_do_not_hold_them_on_x11() {
    early_verbs_do_not_hold_the_settings(false).await;
}

#[tokio::test]
#[ignore = "needs a built selkies server with the Wayland backend"]
async fn verbs_ahead_of_the_initial_settings_do_not_hold_them_on_wayland() {
    early_verbs_do_not_hold_the_settings(true).await;
}
