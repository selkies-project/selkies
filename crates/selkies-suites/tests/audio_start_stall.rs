/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The session's automatic audio start must not hold the input behind it.
//!
//! A primary page's first SETTINGS brings the audio capture up when the session starts with audio
//! on, and that start asks the sound server for the capture sink. A sound server that accepts the
//! connection and then never answers is read to the control layer's own bounds, tens of seconds,
//! so the start runs off the WebSockets receive loop: a pad announced while it is outstanding is
//! associated at once, as it is over the WebRTC data channel, whose audio start was never on its
//! message path. The start holds only the audio lock, so a later SETTINGS, which the receive loop
//! applies inline under the reconfigure lock, goes ahead too, and a pad announced behind it is
//! not held either.

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use selkies_suites::{Checks, Client, private_dir, server};
use serde_json::json;

const SETTINGS_PROCESSED: &str = "settings applied for 'primary'";
const ASSOCIATED: &str = "associated with persistent virtual gamepad slot 0";
const ASSOCIATED_BEHIND_SETTINGS: &str = "associated with persistent virtual gamepad slot 1";
const AUDIO_ATTEMPTED: &str =
    "Initial setup: Primary client connected, audio not active, attempting start.";
/// "Stall Test Pad" and "Stall Test Pad 2", as a pad's name travels.
const PAD: &str = "U3RhbGwgVGVzdCBQYWQ=";
const SECOND_PAD: &str = "U3RhbGwgVGVzdCBQYWQgMg==";
const PROMPT: Duration = Duration::from_secs(5);

/// The SETTINGS a primary page sends.
fn settings(framerate: u32) -> String {
    format!(
        "SETTINGS,{}",
        json!({
            "displayId": "primary",
            "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": false, "framerate": framerate, "encoder": "h264enc",
            "scaling_dpi": 96,
        })
    )
}

/// Listen on `path` as a sound server that accepts every connection and answers nothing, the
/// shape of one that is up and wedged. The connections are held for as long as the returned list
/// lives.
fn silent_sound_server(path: &std::path::Path) -> Arc<Mutex<Vec<UnixStream>>> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).expect("the silent sound server's socket");
    let held = Arc::new(Mutex::new(Vec::new()));
    let keep = Arc::clone(&held);
    std::thread::spawn(move || {
        for connection in listener.incoming().flatten() {
            keep.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(connection);
        }
    });
    held
}

/// How long after `since` the instant `at` fell, for a check's detail.
fn after(at: Option<Instant>, since: Option<Instant>) -> String {
    match (at, since) {
        (Some(at), Some(since)) => format!(
            "{:.1}s after",
            at.saturating_duration_since(since).as_secs_f32()
        ),
        _ => "never".to_owned(),
    }
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_pad_announced_during_a_stalled_audio_start_is_associated_at_once() {
    let socket = private_dir("run").join("pulse-silent.sock");
    let _held = silent_sound_server(&socket);
    let server = server::launch("stall")
        .env("PULSE_SERVER", &format!("unix:{}", socket.display()))
        .env("SELKIES_DEBUG", "true")
        .start()
        .await;
    let mut checks = Checks::new("audio-start-stall");

    let page = Client::connect(&server, "").await;
    page.send(&settings(60));
    page.send(&format!("js,c,0,{PAD},4,17"));
    let processed = server
        .saw_at(0, SETTINGS_PROCESSED, Duration::from_secs(60))
        .await;
    checks.check("the first SETTINGS is processed", processed.is_some(), "");
    let associated = server.saw_at(0, ASSOCIATED, Duration::from_secs(20)).await;
    checks.check(
        "a pad announced during the audio start is associated within 5 s",
        matches!((processed, associated), (Some(p), Some(a)) if a.saturating_duration_since(p) < PROMPT),
        format!("{} the settings", after(associated, processed)),
    );
    checks.check(
        "the audio start was still attempted",
        server
            .saw(0, AUDIO_ATTEMPTED, Duration::from_secs(20))
            .await,
        "",
    );

    page.send(&settings(30));
    let sent = Instant::now();
    page.send(&format!("js,c,1,{SECOND_PAD},4,17"));
    let associated = server
        .saw_at(0, ASSOCIATED_BEHIND_SETTINGS, Duration::from_secs(20))
        .await;
    checks.check(
        "a pad announced behind a later SETTINGS, during the same start, is associated within 5 s",
        associated.is_some_and(|at| at.saturating_duration_since(sent) < PROMPT),
        format!("{} it", after(associated, Some(sent))),
    );
    let log = server.stop().await;
    let _ = std::fs::remove_file(&socket);
    checks.finish(&log);
}
