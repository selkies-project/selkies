/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The wire protocol, driven by hand over the data WebSocket.
//!
//! Settings that change while a stream is stopped must not restart its capture, and the verbs of
//! the WebRTC dialect apply live on the WebSockets transport, sanitized. The `cmd` verb the
//! dashboards' apps panel posts on names bare commands, so the server has to resolve them on its
//! own `PATH`, and a launch that fails has to come back as `command_error` or the optimistic
//! install or remove in the UI never rolls back. A clipboard fetch is answered to the page that
//! asked, and the gamepad slot a shared viewer may drive is the one its own connection carries
//! rather than the index its message names.

use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use selkies_suites::{Checks, Client, Server, path_with, script, server, workdir};
use serde_json::{Value, json};

const CAPTURE_STARTED: &str = "Capture started for 'primary'";
/// What the server logs once a client's pad is bound to a slot, ahead of the slot's index.
const ASSOCIATED: &str = "associated with persistent virtual gamepad slot";

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

/// The file the stand-in `selkies-proot` writes its arguments to.
fn sentinel() -> PathBuf {
    workdir().join("cmd-stub.args")
}

/// A server with the command channel on and a stand-in `selkies-proot` ahead on its `PATH`, and a
/// primary page whose stream is up.
///
/// The server probes the wrapper with `check` before it publishes the apps panel and `list` for
/// what is installed; only a real command is recorded.
async fn session(label: &str) -> (Server, Client) {
    // Written once: the tests of this target start together, and a server of one may be running
    // the stand-in while another sets up.
    static STUB: Once = Once::new();
    let stub = workdir().join("cmd-stub-bin");
    STUB.call_once(|| {
        script(
            &stub.join("selkies-proot"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in check|list) exit 0 ;; esac\necho \"$@\" > \"{}\"\n",
                sentinel().display()
            ),
        );
    });
    let server = server::launch(label)
        .env("SELKIES_COMMAND_ENABLED", "true")
        .env("PATH", &path_with(&stub))
        .start()
        .await;
    let page = Client::connect(&server, "").await;
    page.send(&settings(json!({})));
    server
        .settle(0, "Stream settings active", Duration::from_secs(4))
        .await;
    (server, page)
}

/// The `action` of the first `system` message from index `mark` on that starts with `kind`,
/// empty when none arrives within ten seconds.
async fn command_status(page: &Client, mark: usize, kind: &str) -> String {
    let action = |text: &str| -> Option<String> {
        let body: Value = serde_json::from_str(text.strip_prefix("system,")?).ok()?;
        let action = body.get("action")?.as_str()?;
        action.starts_with(kind).then(|| action.to_owned())
    };
    page.text(mark, Duration::from_secs(10), |text| action(text).is_some())
        .await
        .and_then(|text| action(&text))
        .unwrap_or_default()
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn settings_changed_while_a_stream_is_stopped_wait_for_the_next_start() {
    let mut checks = Checks::new("protocol");
    let (server, page) = session("stopped").await;
    checks.check(
        "protocol: the first message names the transport",
        page.hello().starts_with("MODE "),
        page.hello(),
    );
    checks.check(
        "protocol: capture started from initial SETTINGS",
        server.saw(0, CAPTURE_STARTED, Duration::from_secs(4)).await,
        "",
    );

    let mark = server.mark();
    page.send("STOP_VIDEO");
    checks.check(
        "protocol: STOP_VIDEO stops capture",
        server
            .saw(
                mark,
                "Stopping all streams for display 'primary'",
                Duration::from_secs(8),
            )
            .await,
        "",
    );

    let mark = server.mark();
    page.send(&settings(json!({"framerate": 30})));
    let deferred = server
        .saw(mark, "deferring the restart", Duration::from_secs(8))
        .await;
    checks.check(
        "A1: settings change defers restart while stopped",
        deferred,
        "",
    );
    checks.check(
        "A1: no capture restart while stopped",
        server
            .stays_without(mark, "Capture started for", Duration::from_secs(1))
            .await
            || deferred,
        "",
    );
    checks.check(
        "A1: capture stays stopped after deferral",
        server
            .stays_without(mark, "Capture started", Duration::from_secs(2))
            .await,
        "",
    );

    page.send("START_VIDEO");
    checks.check(
        "protocol: START_VIDEO resumes capture",
        server
            .saw(mark, "Capture started", Duration::from_secs(8))
            .await,
        "",
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn the_webrtc_dialects_verbs_apply_live_and_sanitized() {
    let mut checks = Checks::new("protocol");
    let (server, page) = session("verbs").await;

    let mark = server.mark();
    page.send("_arg_fps,33");
    checks.check(
        "F4: '_arg_fps,33' applied",
        server.saw(mark, "framerate", Duration::from_secs(6)).await,
        "",
    );

    let mark = server.mark();
    page.send("vb,1");
    checks.check(
        "F5: 'vb,1' clamped to range min",
        server
            .saw(mark, "clamped to 100", Duration::from_secs(6))
            .await,
        "",
    );

    let mark = server.mark();
    page.send("vb,3912");
    checks.check(
        "F5: 'vb,3912' live bitrate applied",
        server.saw(mark, "3912", Duration::from_secs(6)).await,
        "",
    );

    // Both transports default to CBR, so CRF is the structural change.
    let mark = server.mark();
    page.send("_rc,crf");
    checks.check(
        "F5: '_rc,crf' triggers structural restart",
        server
            .saw(
                mark,
                "Applied rate-control via '_rc'",
                Duration::from_secs(8),
            )
            .await
            || server
                .saw(
                    mark,
                    "Restarting its capture stream",
                    Duration::from_secs(8),
                )
                .await,
        "",
    );
    // Back to CBR so the run ends in the transport's default mode.
    let mark = server.mark();
    page.send("_rc,cbr");
    checks.check(
        "F5: '_rc,cbr' toggles back to CBR",
        server
            .saw(
                mark,
                "Applied rate-control via '_rc': cbr",
                Duration::from_secs(13),
            )
            .await,
        "",
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn commands_resolve_on_the_servers_path_and_report_how_they_ended() {
    let mut checks = Checks::new("protocol");
    let _ = std::fs::remove_file(sentinel());
    let (server, page) = session("commands").await;

    page.send("cmd,selkies-proot install demo-app");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !sentinel().exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The stub's redirection makes the file before `echo` fills it.
    let mut recorded = String::new();
    while recorded.is_empty() && tokio::time::Instant::now() < deadline {
        recorded = std::fs::read_to_string(sentinel()).unwrap_or_default();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    checks.check(
        "cmd: a bare command name resolves on the server's PATH",
        recorded.trim() == "install demo-app",
        recorded.trim(),
    );

    // command_error carries the echoed command, which is what the dashboards match their pending
    // action against.
    let mark = page.texts();
    page.send("cmd,selkies-proot-absent install demo-app");
    let error = command_status(&page, mark, "command_error,").await;
    checks.check(
        "cmd: a missing command reports command_error to the client",
        error.ends_with(": selkies-proot-absent install demo-app") && error.contains("127"),
        &error,
    );

    // What the command printed is the only thing that says why, and the apps panel shows the
    // notice verbatim.
    let failing = "sh -c 'echo proot error: ptrace denied >&2; exit 3'";
    let mark = page.texts();
    page.send(&format!("cmd,{failing}"));
    let error = command_status(&page, mark, "command_error,").await;
    checks.check(
        "cmd: a failure carries the reason its output gave",
        error.contains("proot error: ptrace denied") && error.ends_with(&format!(": {failing}")),
        &error,
    );

    // A clean exit settles the pending action the panel shows running.
    let mark = page.texts();
    page.send("cmd,true");
    let done = command_status(&page, mark, "command_done,").await;
    checks.check(
        "cmd: a clean exit reports command_done with the command",
        done == "command_done,true",
        &done,
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn the_dpi_and_resize_verbs_reach_the_desktop() {
    let mut checks = Checks::new("protocol");
    let (server, page) = session("desktop").await;

    // The resize and DPI verbs reach the desktop through the dispatch both transports share. A
    // page carries both in its SETTINGS payload, so the bare verbs are exercised here.
    let mark = server.mark();
    page.send("s,144");
    checks.check(
        "s: a DPI sync from the wire scales the desktop",
        server
            .saw(mark, "Successfully set DPI to 144", Duration::from_secs(8))
            .await,
        "",
    );

    let mark = server.mark();
    page.send("r,1600x900");
    checks.check(
        "r: a resize naming no display takes the connection's",
        server
            .saw(
                mark,
                "for display 'primary' with resolution: 1600x900",
                Duration::from_secs(8),
            )
            .await,
        "",
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_clipboard_fetch_is_answered_to_its_requester_alone() {
    let mut checks = Checks::new("protocol");
    let (server, page) = session("clipboard").await;

    // A bystander that receives a fetch reply reads it as its own and caches the content
    // unwritten, suppressing the next real change. It joins as a viewer: a second controller
    // would take the display over.
    let other = Client::connect(&server, "?role=viewer").await;
    let mark = other.texts();
    page.send("cr");
    // Read for a fixed window rather than to a gap: a viewer is being streamed to, so there is
    // always another frame coming.
    let stray = other
        .texts_during(mark, Duration::from_secs(3), |text| {
            text.starts_with("clipboard")
        })
        .await;
    checks.check(
        "clipboard: a fetch is answered to its requester alone",
        stray.is_empty(),
        format!("{:?}", stray.iter().take(2).collect::<Vec<_>>()),
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_viewer_drives_only_the_gamepad_slot_its_connection_carries() {
    let mut checks = Checks::new("protocol");
    let (server, _page) = session("gamepad").await;

    // Each probe waits on the verdict it expects, then reads the association line, which names
    // the slot actually driven, out of that same window.
    for (label, query, index, allowed) in [
        (
            "a shared viewer cannot drive player 1",
            "?role=viewer",
            0,
            false,
        ),
        (
            "a shared viewer cannot drive player 2",
            "?role=viewer",
            1,
            false,
        ),
        (
            "a player-2 viewer cannot drive player 1",
            "?role=viewer&slot=2",
            0,
            false,
        ),
        (
            "a player-2 viewer drives its own slot",
            "?role=viewer&slot=2",
            1,
            true,
        ),
    ] {
        let mark = server.mark();
        let viewer = Client::connect(&server, query).await;
        viewer.send(&format!("js,c,{index},UFJPQkU=,6,17"));
        viewer.send(&format!("js,b,{index},0,1"));
        let association = format!("{ASSOCIATED} {index}");
        let verdict = if allowed {
            association.as_str()
        } else {
            "DENIED gamepad input"
        };
        let settled = server.saw(mark, verdict, Duration::from_secs(5)).await;
        viewer.close().await;
        let reached = server.log_from(mark).contains(&association);
        checks.check(
            &format!("gamepad: {label}"),
            settled && reached == allowed,
            format!(
                "index {index} {}",
                if reached { "reached" } else { "refused" }
            ),
        );
    }
    checks.finish(&server.stop().await);
}
