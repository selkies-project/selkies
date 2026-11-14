/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A live session's transfers, as an audit collector sees them.
//!
//! Every channel is driven the way a client drives it (an upload and a download over HTTP, a
//! paste into the session and a copy out of it over the WebSocket) against a real collector, and
//! each one has to arrive as one event carrying metadata and no content. The collector is then
//! stalled to prove what the queued sender buys: a transfer, and the stream, run at their own
//! speed whatever the collector does with the POST.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use selkies_suites::http::{self, Collector, Posted};
use selkies_suites::server::test_display;
use selkies_suites::x11::{Damage, SelectionOwner};
use selkies_suites::{Checks, Client, Server, server, workdir};
use serde_json::{Value, json};

const TOKEN: &str = "audit-integration-token";
const PASTED: &[u8] = b"pasted into the session";
/// [`PASTED`], as the `cw` verb carries it.
const PASTED_BASE64: &str = "cGFzdGVkIGludG8gdGhlIHNlc3Npb24=";
const COPIED: &[u8] = b"copied out of the session";
/// 64 KiB, the size of every file moved.
const FILE: usize = 64 * 1024;

/// A file's worth of bytes that no compressor flattens.
fn noise() -> Vec<u8> {
    let mut state: u32 = 0x2545_F491;
    (0..FILE)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state.to_be_bytes()[0]
        })
        .collect()
}

/// An empty folder for the file manager, named after `test`.
fn files(test: &str) -> PathBuf {
    let folder = workdir().join(format!("audit-files-{test}"));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).expect("the file manager's folder");
    folder
}

/// A server whose file manager is `folder`, reporting to `collector` when one is given.
async fn audited(label: &str, folder: &Path, collector: Option<&Collector>) -> Server {
    let mut launch =
        server::launch(label).env("SELKIES_FILE_MANAGER_PATH", &folder.display().to_string());
    if let Some(collector) = collector {
        launch = launch
            .env("SELKIES_AUDIT_WEBHOOK_URL", &collector.url("/audit"))
            .env("SELKIES_AUDIT_WEBHOOK_TOKEN", TOKEN)
            .env("SELKIES_AUDIT_WEBHOOK_TIMEOUT", "1.0");
    }
    launch.start().await
}

/// The status of an upload of `data` as `name`; 0 when the request failed.
async fn upload(server: &Server, name: &str, data: &[u8]) -> u16 {
    http::request(
        server.port(),
        "POST",
        "/api/upload",
        &[
            ("Content-Type", "application/octet-stream"),
            ("X-Upload-Path", name),
        ],
        data,
        Duration::from_secs(60),
    )
    .await
    .map_or(0, |response| response.status)
}

/// The events in `posted`, parsed.
fn events(posted: &[Posted]) -> Vec<Value> {
    posted
        .iter()
        .filter_map(|post| serde_json::from_slice(&post.body).ok())
        .collect()
}

/// How many of `posted` are the event `kind`.
fn count(posted: &[Posted], kind: &str) -> usize {
    events(posted)
        .iter()
        .filter(|event| event["event"] == kind)
        .count()
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn every_transfer_reaches_the_collector_as_metadata_alone() {
    let collector = Collector::start().await;
    let folder = files("channels");
    let server = audited("channels", &folder, Some(&collector)).await;
    let mut checks = Checks::new("audit-webhook");
    let body = noise();

    checks.check(
        "an upload lands",
        upload(&server, "audit/landed.bin", &body).await == 200,
        "",
    );
    checks.check(
        "a traversal attempt is refused",
        upload(&server, "../escaped.bin", &body).await == 400,
        "",
    );
    let served = http::request(
        server.port(),
        "GET",
        "/api/files/audit/landed.bin",
        &[],
        &[],
        Duration::from_secs(60),
    )
    .await;
    checks.check(
        "a download is served",
        matches!(served, Ok(ref response) if response.status == 200 && response.body == body),
        served.map_or_else(|why| why, |response| response.status.to_string()),
    );

    // Paste into the session, then ask for the session's own clipboard back.
    let page = Client::connect(&server, "").await;
    page.send(&format!("cw,{PASTED_BASE64}"));
    // The server's write of the paste comes back to its clipboard monitor as a selection
    // change, and a change the monitor handles once another client owns the selection is that
    // client's copy: the copy is taken only after the paste's change has been handled, which
    // nothing reports, so after the second a session's clipboard takes to settle.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    {
        let _owner = SelectionOwner::serving(&test_display(), COPIED);
        page.send("cr");
        collector
            .received_when(Duration::from_millis(2500), |posted| {
                count(posted, "clipboard.send") >= 2
            })
            .await;
    }
    page.close().await;

    let posted = collector
        .received_when(Duration::from_secs(15), |posted| posted.len() >= 7)
        .await;
    let got = events(&posted);
    let by_event = |kind: &str| {
        got.iter()
            .find(|event| event["event"] == kind)
            .cloned()
            .unwrap_or(Value::Null)
    };
    // The page of the clipboard round trip connects and disconnects; its disconnect may land
    // after the wait.
    let mut kinds: Vec<&str> = got
        .iter()
        .filter_map(|event| event["event"].as_str())
        .filter(|kind| *kind != "session.disconnect")
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    checks.check(
        "every channel is recorded",
        kinds
            == [
                "clipboard.receive",
                "clipboard.send",
                "file.download",
                "file.upload.end",
                "file.upload.error",
                "session.connect",
            ],
        format!("{kinds:?}"),
    );
    let landed = by_event("file.upload.end");
    checks.check(
        "the upload carries its path and size",
        landed["filename"] == "audit/landed.bin" && landed["size_bytes"] == json!(FILE),
        &landed,
    );
    let refused = by_event("file.upload.error");
    checks.check(
        "the refusal carries the path as it was asked for",
        refused["filename"] == "../escaped.bin",
        &refused,
    );
    let downloaded = by_event("file.download");
    checks.check(
        "the download carries its path and size",
        downloaded["filename"] == "audit/landed.bin" && downloaded["size_bytes"] == json!(FILE),
        &downloaded,
    );
    let pasted = by_event("clipboard.receive");
    checks.check(
        "the paste is recorded with its size",
        pasted["size_bytes"] == json!(PASTED.len()),
        &pasted,
    );
    checks.check(
        "the paste is not recorded as content when it is announced back to the clients",
        !got.iter().any(|event| {
            let text = event.to_string();
            text.contains("pasted") || text.contains("copied")
        }),
        got.first().unwrap_or(&Value::Null),
    );
    // The paste is not announced back (the monitor knows the server wrote it), so the two sends
    // are the announcement of the copy and the reply to the request.
    let sends: Vec<&Value> = got
        .iter()
        .filter(|event| event["event"] == "clipboard.send")
        .map(|event| &event["size_bytes"])
        .collect();
    checks.check(
        "the copy leaving the session is recorded each time it is sent",
        sends == [&json!(COPIED.len()), &json!(COPIED.len())],
        format!("{sends:?}"),
    );
    checks.check(
        "every POST carries the configured token",
        posted
            .iter()
            .all(|post| post.header("authorization") == Some(&format!("Bearer {TOKEN}"))),
        format!("{:?}", posted.first().map(|post| &post.headers)),
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_stalled_collector_does_not_pace_the_transfers() {
    let collector = Collector::start().await;
    let folder = files("stalled");
    let server = audited("stalled", &folder, Some(&collector)).await;
    let mut checks = Checks::new("audit-webhook");
    let body = noise();

    // What the queue buys: the collector holds every POST open, and the transfers still run at
    // the speed they run at when it answers.
    let started = Instant::now();
    checks.check(
        "a second upload lands",
        upload(&server, "audit/second.bin", &body).await == 200,
        "",
    );
    let answering = started.elapsed();
    collector.stall(true);
    let started = Instant::now();
    checks.check(
        "an upload lands while the collector stalls",
        upload(&server, "audit/stalled.bin", &body).await == 200,
        "",
    );
    let status = http::get(server.port(), "/api/files/audit/stalled.bin")
        .await
        .map_or(0, |response| response.status);
    let stalled = started.elapsed();
    checks.check(
        "a download is served while the collector stalls",
        status == 200,
        status,
    );
    checks.check(
        "a stalled collector does not pace the transfers",
        stalled < (answering * 4).max(Duration::from_secs(1)),
        format!("{stalled:.2?} stalled vs {answering:.2?} answering"),
    );
    collector.stall(false);
    checks.finish(&server.stop().await);
}

/// Frames a page receives over eight seconds while it pastes into the session throughout,
/// acking each. Each paste is one audit event, so a run against a collector that never answers
/// is what a POST on the event loop would show up in.
async fn frames_while_pasting(server: &Server) -> usize {
    let page = Client::connect(server, "").await;
    page.send(&format!(
        "SETTINGS,{}",
        json!({
            "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": false, "framerate": 60, "video_crf": 25,
            "video_bitrate": 6000, "audio_bitrate": 128000, "scaling_dpi": 96,
        })
    ));
    let (mut seen, mut last, mut read) = (0, None, 0);
    let end = Instant::now() + Duration::from_secs(8);
    let mut paste_at = Instant::now();
    while Instant::now() < end {
        for frame in page.binaries(read) {
            read += 1;
            let id = frame.video_frame_id();
            if id.is_some() && id != last {
                last = id;
                seen += 1;
                page.send(&format!("CLIENT_FRAME_ACK {} 0", id.unwrap_or_default()));
            }
        }
        if Instant::now() >= paste_at {
            page.send(&format!("cw,{PASTED_BASE64}"));
            paste_at = Instant::now() + Duration::from_millis(200);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    page.close().await;
    seen
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn the_audit_channel_costs_the_stream_no_frames() {
    let collector = Collector::start().await;
    let folder = files("frames");
    let mut checks = Checks::new("audit-webhook");
    // The stream's own rate, with the channel off and then on against a collector that never
    // answers: an event that reached the loop would cost frames here.
    let server = audited("quiet", &folder, None).await;
    let motion = Damage::start(&test_display());
    let quiet = frames_while_pasting(&server).await;
    let mut logs = server.stop().await;
    collector.stall(true);
    let server = audited("audited", &folder, Some(&collector)).await;
    let with_audit = frames_while_pasting(&server).await;
    drop(motion);
    collector.stall(false);
    checks.check(
        "the audit channel costs the stream no frames",
        quiet > 100 && with_audit * 10 >= quiet * 9,
        format!(
            "{with_audit} frames audited against a stalled collector, {quiet} with the channel off"
        ),
    );
    logs.push_str(&server.stop().await);
    checks.finish(&logs);
}
