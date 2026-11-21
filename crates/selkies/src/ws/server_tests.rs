/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The service over stand-ins for what it drives: the handshake, the verbs, the owner model,
//! the fan-out, and the teardown.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde_json::{Value as Json, json};

use crate::core::PRIMARY_DISPLAY;
use crate::ws::client::testing::FakeSocket;
use crate::ws::clock_tests::{advance, reset_clock};
use crate::ws::host::{CaptureHandle, KeyFrameCause, TokenEntry};
use crate::ws::protocol::gzip_frame;
use crate::ws::relay::RelayHost;
use crate::ws::testkit::*;

async fn settle() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

/// Whether `capture` is given `call` within a second: a call a timer makes.
async fn given(capture: &FakeCapture, call: &str) -> bool {
    for _ in 0..100 {
        if capture.has(call) {
            return true;
        }
        settle().await;
    }
    false
}

fn entry(role: &str, slot: Json) -> TokenEntry {
    TokenEntry {
        role: role.into(),
        slot,
    }
}

#[tokio::test]
async fn the_handshake_pushes_in_order() {
    let rig = Rig::new();
    let page = rig.controller().await;
    let texts = page.texts().await;
    assert_eq!(texts[0], "MODE websockets");
    assert!(
        texts[1].starts_with("DISPLAY_CONFIG_UPDATE,"),
        "{}",
        texts[1]
    );
    let payload: Json =
        serde_json::from_str(texts[1].strip_prefix("DISPLAY_CONFIG_UPDATE,").unwrap()).unwrap();
    assert_eq!(payload["type"], json!("display_config_update"));
    assert_eq!(payload["wayland"], json!(false));
    assert!(
        payload.get("layouts").is_none(),
        "layouts only where one exists"
    );
    let settings = typed(&texts, "server_settings").expect("server_settings");
    assert_eq!(
        settings["settings"]["ws_max_message_bytes"]["value"],
        json!(8 * 1024 * 1024)
    );
    assert!(
        settings.get("displayId").is_none(),
        "the handshake's is built for the primary and names none"
    );
}

#[tokio::test]
async fn secure_mode_authenticates_first() {
    let rig = Rig::new();
    *rig.tokens.table.lock() = Some(vec![("good".into(), entry("controller", Json::Null))]);
    rig.service.st.lock().last_connections.clear();
    assert!(
        rig.connect("bad", "", None).await.is_none(),
        "an unknown token ends the connection"
    );
    let page = rig
        .connect("good", "viewer", Some("3"))
        .await
        .expect("a provisioned token connects");
    let texts = page.texts().await;
    assert_eq!(
        texts[0],
        r#"AUTH_SUCCESS,{"role":"controller","slot":null}"#
    );
    assert_eq!(texts[1], "MODE websockets");
    assert_eq!(texts[2], "MK_ACCESS,1");
    assert_eq!(
        page.client().perms().role,
        "controller",
        "the token governs, not the query"
    );
}

#[tokio::test]
async fn a_second_connection_from_one_address_is_debounced() {
    let rig = Rig::new();
    let first = rig.connect("", "", None).await;
    assert!(first.is_some());
    assert!(rig.connect("", "", None).await.is_none(), "closed 4029");
    advance(0.6);
    assert!(
        rig.connect("", "", None).await.is_some(),
        "past the 500 ms debounce"
    );
    reset_clock();
}

#[tokio::test]
async fn gz_is_echoed_and_a_gzip_frame_dispatches_as_text() {
    let rig = Rig::new();
    let mut page = rig.owner("a").await;
    page.clear().await;
    page.say("_gz,1").await;
    assert!(page.has("_gz,1").await);
    assert!(page.client().gz());
    let _ = page.conn.handle_binary(gzip_frame(b"kd,65")).await;
    settle().await;
    assert!(
        rig.input.messages.lock().iter().any(|m| m.0 == "kd,65"),
        "inflated into the input dialect"
    );
    let before = rig.input.messages.lock().len();
    let _ = page
        .conn
        .handle_binary(Bytes::from_static(&[0x05, 1, 2, 3]))
        .await;
    let _ = page.conn.handle_binary(Bytes::new()).await;
    assert_eq!(
        rig.input.messages.lock().len(),
        before,
        "an undecodable or empty frame is dropped"
    );
}

#[tokio::test]
async fn a_sent_probe_is_echoed_to_any_role() {
    let rig = Rig::new();
    let mut page = rig.owner("a").await;
    let mut viewer = rig.viewer().await;
    page.clear().await;
    viewer.clear().await;
    page.say("_sent,7").await;
    viewer.say("_sent,8").await;
    assert!(page.has("_sent,7").await);
    assert!(viewer.has("_sent,8").await);
    page.clear().await;
    page.say("_sent,x").await;
    page.say("_sent,-1").await;
    settle().await;
    assert!(
        !page.texts().await.iter().any(|t| t.starts_with("_sent,")),
        "only a counter is echoed"
    );
    assert!(rig.input.messages.lock().is_empty());
}

#[tokio::test]
async fn a_viewer_sends_only_what_it_may() {
    let rig = Rig::new();
    let mut viewer = rig.viewer().await;
    for verb in [
        "kd,65",
        "m,1,2,0,0",
        "cw,aGk=",
        "cmd,xterm",
        "kr",
        "cr",
        "_pointer_echo,1",
        "vb,100",
    ] {
        viewer.say(verb).await;
    }
    assert!(
        rig.input.messages.lock().is_empty(),
        "{:?}",
        rig.input.messages.lock()
    );
    viewer.say("js,b,0,1,1").await;
    assert!(rig.input.messages.lock().is_empty(), "no slot, no gamepad");
    viewer.say("REQUEST_KEYFRAME").await;
    let _ = viewer
        .conn
        .handle_binary(Bytes::from_static(&[0x02, 9, 9]))
        .await;
    assert!(viewer.has("MICROPHONE_DISABLED").await);
    let _ = viewer
        .conn
        .handle_binary(Bytes::from_static(&[0x06, 0, 0, 9]))
        .await;
    assert!(viewer.has("WEBCAM_DISABLED").await);
    viewer.clear().await;
    let _ = viewer
        .conn
        .handle_binary(Bytes::from_static(&[0x02, 9, 9]))
        .await;
    assert!(
        !viewer.has("MICROPHONE_DISABLED").await,
        "told once per connection"
    );
}

#[tokio::test]
async fn a_controllers_first_settings_lays_the_display_out_and_starts_it() {
    let rig = Rig::new();
    let page = rig.owner("a").await;
    settle().await;
    assert_eq!(
        rig.layout(PRIMARY_DISPLAY).map(|l| (l.w, l.h)),
        Some((1280, 720))
    );
    let capture = rig.primary_capture();
    assert!(capture.has("start 1280x720"), "{:?}", capture.calls.lock());
    let texts = page.texts().await;
    assert!(texts.iter().any(|t| t == "PAGE_VISIBILITY"));
    let resolution = typed(&texts, "stream_resolution").expect("stream_resolution");
    assert_eq!(
        resolution,
        json!({"type": "stream_resolution", "width": 1280, "height": 720, "displayId": "primary"})
    );
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("DISPLAY_CONFIG_UPDATE,") && t.contains("\"layouts\""))
    );
    assert!(
        rig.display
            .calls
            .lock()
            .iter()
            .any(|c| c == "layout 1280x720"),
        "the total is framebuffer aligned by 8"
    );
    let summary = rig
        .service
        .settings_applied_summary("peer", PRIMARY_DISPLAY);
    assert!(
        summary.contains("settings applied for 'primary': 1280x720, h264enc"),
        "{summary}"
    );
}

#[tokio::test]
async fn resizes_wait_for_a_display_and_dimensions_are_made_even() {
    let rig = Rig::new();
    let mut page = rig.controller().await;
    page.say("r,800x600").await;
    assert!(
        rig.input.messages.lock().is_empty(),
        "a page on no display has nothing to resize"
    );
    page.say(r#"SETTINGS,{"initialClientWidth":1281,"initialClientHeight":721,"tabId":"a"}"#)
        .await;
    assert_eq!(
        rig.layout(PRIMARY_DISPLAY).map(|l| (l.w, l.h)),
        Some((1280, 720))
    );
    rig.service.handle_resize("1024x768", PRIMARY_DISPLAY).await;
    assert_eq!(
        rig.layout(PRIMARY_DISPLAY).map(|l| (l.w, l.h)),
        Some((1024, 768))
    );
    rig.service
        .handle_resize("not a size", PRIMARY_DISPLAY)
        .await;
    assert_eq!(
        rig.layout(PRIMARY_DISPLAY).map(|l| (l.w, l.h)),
        Some((1024, 768))
    );
}

#[tokio::test]
async fn frames_fan_out_as_pixelflux_framed_them_and_acks_measure_the_owner() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    let mut viewer = rig.viewer().await;
    viewer.say("SETTINGS,{}").await;
    settle().await;
    let capture = rig.primary_capture();
    capture.deliver(1, 1, true);
    settle().await;
    capture.deliver(2, 1, false);
    settle().await;
    for page in [&owner, &viewer] {
        let frames = page.binaries();
        assert_eq!(frames.len(), 2, "one message per frame");
        assert_eq!(&frames[0][..4], &[0x04, 0x11, 0, 1]);
        assert_eq!(&frames[1][..4], &[0x04, 0x10, 0, 2]);
        assert_eq!(
            frames[1].len(),
            112,
            "the header is pixelflux's, sent as is"
        );
    }
    let link = rig.service.link(PRIMARY_DISPLAY).unwrap();
    assert_eq!(
        link.lock().last_sent_frame_id,
        2,
        "the owner's relay stamps"
    );
    viewer.say("CLIENT_FRAME_ACK 2").await;
    assert_eq!(
        link.lock().acknowledged_frame_id,
        -1,
        "a viewer's ack is not the owner's"
    );
    owner.say("CLIENT_FRAME_ACK 2 1.5").await;
    assert_eq!(link.lock().acknowledged_frame_id, 2);
    assert!(link.lock().rtt_floor_ms.is_some());
    // The page's clock as the ack left and its count ride behind the hold, and an ack is one
    // without them, with one of them, or with ones that say nothing.
    for ack in [
        "CLIENT_FRAME_ACK 2 40 1763700000123.4 5000",
        "CLIENT_FRAME_ACK 2 40 1763700000123.4",
        "CLIENT_FRAME_ACK 2 40 x 5000",
        "CLIENT_FRAME_ACK 2 40 1763700000123.4 -1",
    ] {
        link.lock().acknowledged_frame_id = -1;
        owner.say(ack).await;
        assert_eq!(link.lock().acknowledged_frame_id, 2, "{ack}");
    }
    for bad in [
        "CLIENT_FRAME_ACK",
        "CLIENT_FRAME_ACK -1",
        "CLIENT_FRAME_ACK 65536",
        "CLIENT_FRAME_ACK x",
    ] {
        owner.say(bad).await;
        assert_eq!(link.lock().acknowledged_frame_id, 2, "{bad} is refused");
    }
}

/// The bytes of the binary messages a page was sent: its count, having got them all.
fn counted(page: &RigPage) -> u64 {
    page.binaries().iter().map(|b| b.len() as u64).sum()
}

/// An ack of frame `id` that leaves now, the page's clock an hour from this one, with
/// `received` bytes counted.
fn stamped(id: u16, received: u64) -> String {
    let stamp = (crate::ws::now() + 3600.0) * 1000.0;
    format!("CLIENT_FRAME_ACK {id} 0 {stamp:.1} {received}")
}

#[tokio::test]
async fn only_the_displays_own_page_has_its_video_held() {
    let rig = Rig::new();
    let mut owner = rig.written("a").await;
    let mut beside = rig.written("b").await;
    settle().await;
    {
        let reg = rig.service.reg.lock();
        assert!(reg.owns(owner.id(), PRIMARY_DISPLAY) && reg.is_beside(beside.id()));
    }
    let capture = rig.primary_capture();
    // Both take two frames and ack each with their clock and their count on it.
    for id in 1..=2u16 {
        capture.deliver(id, 1, id == 1);
        tokio::time::sleep(Duration::from_millis(40)).await;
        owner.say(&stamped(id, counted(&owner))).await;
        beside.say(&stamped(id, counted(&beside))).await;
    }
    assert_eq!((owner.binaries().len(), beside.binaries().len()), (2, 2));
    // The third stands out past the bound, by either page's word of an ack it repeats. The
    // second tab's ack is not its display's: nothing counts the frames a writer would hold
    // for it, so its word paces nothing.
    let had = (counted(&owner), counted(&beside));
    capture.deliver(3, 1, false);
    tokio::time::sleep(Duration::from_millis(160)).await;
    owner.say(&stamped(2, had.0)).await;
    beside.say(&stamped(2, had.1)).await;
    capture.deliver(4, 1, false);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!((owner.binaries().len(), beside.binaries().len()), (3, 4));
    assert!(owner.client().paced() && !beside.client().paced());
}

/// The pings of the socket's round-trip clock a page was sent (`ws::uplink`'s, by their
/// counters), the ones a pong of which acks bytes for the steer.
fn clock_pings(page: &RigPage) -> usize {
    page.page
        .sent
        .lock()
        .iter()
        .filter(
            |f| matches!(f, crate::ws::client::Frame::Ping(p) if p.len() == 8 && p[0] >> 6 == 2),
        )
        .count()
}

#[tokio::test]
async fn padding_is_pinged_only_with_no_frame_awaiting_its_ack() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    let link = rig.service.link(PRIMARY_DISPLAY).unwrap();
    capture.deliver(1, 1, true);
    settle().await;
    owner.say("CLIENT_FRAME_ACK 1 0").await;
    capture.deliver(2, 1, false);
    settle().await;
    // The page repeats frame 1's ack on its next tick: it is alive, and frame 2 is still
    // out. A ping behind padding now would have its pong ack bytes past frame 2 first.
    owner.say("CLIENT_FRAME_ACK 1 30").await;
    {
        let l = link.lock();
        assert!(l.unacked_since.is_none() && l.awaits_ack());
    }
    let relay = {
        let reg = rig.service.reg.lock();
        reg.relays
            .get(PRIMARY_DISPLAY)
            .and_then(|group| group.get(&owner.id()).cloned())
            .expect("the owner's relay")
    };
    let padding = |page: &RigPage| page.binaries().iter().filter(|b| b[0] == 0x07).count();
    let before = clock_pings(&owner);
    relay.set_padding(800_000.0, crate::ws::now() + 10.0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(padding(&owner) >= 3, "{} chunks", padding(&owner));
    assert_eq!(clock_pings(&owner), before, "frame 2 awaits its ack");
    owner.say("CLIENT_FRAME_ACK 2 0").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        clock_pings(&owner) > before,
        "nothing awaits an ack any more"
    );
    relay.set_padding(0.0, 0.0);
}

#[tokio::test]
async fn the_gate_counts_the_frames_the_writer_holds_back_from_a_late_path() {
    let rig = Rig::new();
    let mut owner = rig.written("a").await;
    settle().await;
    let capture = rig.primary_capture();
    let link = rig.service.link(PRIMARY_DISPLAY).unwrap();
    // The page takes two frames and acks each with its clock and its count on it.
    for id in 1..=2u16 {
        capture.deliver(id, 1, id == 1);
        tokio::time::sleep(Duration::from_millis(40)).await;
        owner.say(&stamped(id, counted(&owner))).await;
    }
    assert_eq!(owner.binaries().len(), 2);
    // The third stands out past the bound, by the page's word of an ack it repeats.
    let had = counted(&owner);
    capture.deliver(3, 1, false);
    tokio::time::sleep(Duration::from_millis(160)).await;
    owner.say(&stamped(2, had)).await;
    // The writer holds the next back, and thirty more wait behind it with the relay.
    for id in 4..=34u16 {
        capture.deliver(id, 1, false);
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(owner.binaries().len(), 3, "held back from the late path");
    assert!(owner.client().paced());
    assert!(link.lock().backpressure_enabled);
    // Two frames are out past the newest acked, far under what the gate allows: it shuts
    // for the frames that wait, which only the writer's pacing has it count.
    let at = crate::ws::now() + 2.0;
    for k in 0..2 {
        rig.service
            .backpressure_check(PRIMARY_DISPLAY, &link, at + f64::from(k) * 0.5, 0.0)
            .await;
    }
    let l = link.lock();
    assert!(
        !l.backpressure_enabled && l.gate_counts.triggered == 1,
        "{:?}",
        l.gate_counts
    );
}

#[tokio::test]
async fn a_paused_or_secondary_socket_leaves_the_primary_fan_out() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    let mut viewer = rig.viewer().await;
    viewer.say("SETTINGS,{}").await;
    viewer.say("STOP_VIDEO").await;
    assert!(viewer.has("VIDEO_STOPPED").await);
    let capture = rig.primary_capture();
    assert!(capture.is_capturing_now(), "the owner still consumes it");
    capture.deliver(1, 1, true);
    settle().await;
    assert!(viewer.binaries().is_empty());
}

#[tokio::test]
async fn the_last_unpaused_consumer_stops_the_capture_and_the_reset_is_sent_once() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    owner.clear().await;
    owner.say("STOP_VIDEO").await;
    let texts = owner.texts().await;
    assert_eq!(
        texts
            .iter()
            .filter(|t| *t == "PIPELINE_RESETTING primary")
            .count(),
        1,
        "{texts:?}"
    );
    assert!(texts.iter().any(|t| t == "VIDEO_STOPPED"));
    assert!(!rig.primary_capture().is_capturing_now());
    owner.clear().await;
    owner.say("START_VIDEO").await;
    assert!(owner.has("VIDEO_STARTED").await);
    assert!(rig.primary_capture().is_capturing_now());
}

#[tokio::test]
async fn a_viewers_start_video_is_throttled_and_a_resume_deferred() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    let mut viewer = rig.viewer().await;
    viewer.say("SETTINGS,{}").await;
    viewer.clear().await;
    viewer.say("START_VIDEO").await;
    assert!(
        viewer.has("PIPELINE_RESETTING primary").await,
        "a shared client needs a decode entry point"
    );
    viewer.clear().await;
    viewer.say("START_VIDEO").await;
    assert!(
        !viewer.has("PIPELINE_RESETTING primary").await,
        "throttled within five seconds"
    );
    viewer.say("STOP_VIDEO").await;
    viewer.clear().await;
    viewer.say("START_VIDEO").await;
    assert!(
        rig.service.reg.lock().video_paused.contains(&viewer.id()),
        "a rapid resume stays paused this cycle"
    );
    assert!(
        rig.service.reg.lock().waking.contains(&viewer.id()),
        "and counts as a consumer while it wakes"
    );
    assert!(!viewer.has("PIPELINE_RESETTING primary").await);
}

#[tokio::test]
async fn lost_frames_and_key_frames_are_floored() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    let mut viewer = rig.viewer().await;
    settle().await;
    let capture = rig.primary_capture();
    owner.say("LOST_FRAME 70000").await;
    assert!(capture.has(&format!("invalidate {} false", 70000 & 0xFFFF)));
    owner.say("LOST_FRAME 71").await;
    assert!(
        !capture.has("invalidate 71 false"),
        "one invalidation per display per 5 ms"
    );
    advance(0.01);
    owner.say("LOST_FRAME 72").await;
    assert!(capture.has("invalidate 72 false"));
    owner.say("LOST_FRAME nope").await;
    let idrs = capture.count("idr");
    owner.say("REQUEST_KEYFRAME").await;
    owner.say("REQUEST_KEYFRAME").await;
    assert_eq!(capture.count("idr"), idrs + 1, "one per display per 250 ms");
    advance(0.3);
    owner.say("REQUEST_KEYFRAME").await;
    assert_eq!(capture.count("idr"), idrs + 2);
    advance(0.3);
    viewer.say("REQUEST_KEYFRAME").await;
    advance(0.3);
    viewer.say("REQUEST_KEYFRAME").await;
    assert_eq!(
        capture.count("idr"),
        idrs + 3,
        "a page that does not own the display repairs once a second"
    );
    reset_clock();
}

/// A lost frame a floor holds back reaches the encoder once the floor ends, the oldest of those
/// held back standing for the rest: the encoder predicts past a frame and every frame after it,
/// so the later reports alone would leave it predicting from the first, which the page dropped.
#[tokio::test]
async fn a_lost_frame_held_back_by_the_floor_is_told_when_it_ends() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    owner.say("LOST_FRAME 10").await;
    owner.say("LOST_FRAME 20").await;
    owner.say("LOST_FRAME 21").await;
    assert!(capture.has("invalidate 10 false"));
    assert!(
        !capture.has("invalidate 20 false"),
        "one invalidation per display per 5 ms"
    );
    assert!(
        given(&capture, "invalidate 20 false").await,
        "the first frame held back is told once the floor ends"
    );
    assert!(
        !capture.has("invalidate 21 false"),
        "the first stands for the frames after it"
    );
}

/// A report from a page beside the owner that the display's floor holds back, another page
/// having just reported, is told once that floor ends, and spends the page's own floor only then.
#[tokio::test]
async fn a_page_beside_the_owner_held_back_by_the_display_floor_is_told_when_it_ends() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    let mut beside = rig.controller().await;
    beside
        .say(r#"SETTINGS,{"tabId":"b","initialClientWidth":640,"initialClientHeight":480}"#)
        .await;
    settle().await;
    assert!(rig.service.reg.lock().is_beside(beside.id()));
    let capture = rig.primary_capture();
    owner.say("LOST_FRAME 50").await;
    beside.say("LOST_FRAME 48").await;
    assert!(capture.has("invalidate 50 false"));
    assert!(
        !capture.has("invalidate 48 false"),
        "one invalidation per display per 5 ms"
    );
    assert!(
        given(&capture, "invalidate 48 false").await,
        "the page's report is told once the display's floor ends"
    );
}

/// A report a page's own floor holds back is dropped and leaves that floor where it was: most
/// such reports name frames encoded before the page's last report reached the encoder, which
/// that report already covers, so the next run the page reports past the floor is told at once.
#[tokio::test]
async fn a_report_the_pages_own_floor_holds_back_does_not_extend_it() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    let mut beside = rig.controller().await;
    beside
        .say(r#"SETTINGS,{"tabId":"b","initialClientWidth":640,"initialClientHeight":480}"#)
        .await;
    settle().await;
    assert!(rig.service.reg.lock().is_beside(beside.id()));
    let capture = rig.primary_capture();
    beside.say("LOST_FRAME 30").await;
    assert!(capture.has("invalidate 30 false"));
    advance(0.99);
    beside.say("LOST_FRAME 31").await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    advance(0.2);
    beside.say("LOST_FRAME 50").await;
    assert!(
        capture.has("invalidate 50 false"),
        "the run past the floor is told at once"
    );
    assert!(!capture.has("invalidate 31 false"));
    reset_clock();
}

/// A page asking for a key frame holds none of the frames sent before the one it asked for:
/// its decoder waits for that one, so the encoder is told of no frame until the page holds it.
#[tokio::test]
async fn a_page_asking_for_a_key_frame_holds_nothing_until_it_takes_one() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    capture.deliver(1, 1, true);
    settle().await;
    capture.deliver(2, 1, false);
    settle().await;
    assert!(
        capture.has("acknowledge 2 true"),
        "the page holds what its relay wrote"
    );
    owner.say("REQUEST_KEYFRAME").await;
    capture.deliver(3, 2, false);
    settle().await;
    assert!(
        !capture.has("acknowledge 3 true"),
        "nor what its relay writes while it waits for the key frame"
    );
    assert!(!capture.has("acknowledge 3 false"));
    capture.deliver(4, 4, true);
    settle().await;
    capture.deliver(5, 4, false);
    settle().await;
    assert!(
        capture.has("acknowledge 5 true"),
        "the key frame starts its holdings afresh"
    );
}

/// A key frame request the floors refuse leaves the page counted as holding what its relay
/// writes: only a request taken brings the key frame that counts the page again.
#[tokio::test]
async fn a_key_frame_request_the_floor_refuses_leaves_the_page_counted() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    let mut beside = rig.controller().await;
    beside
        .say(r#"SETTINGS,{"tabId":"b","initialClientWidth":640,"initialClientHeight":480}"#)
        .await;
    settle().await;
    assert!(rig.service.reg.lock().is_beside(beside.id()));
    let capture = rig.primary_capture();
    capture.deliver(1, 1, true);
    settle().await;
    owner.say("REQUEST_KEYFRAME").await;
    capture.deliver(2, 2, true);
    settle().await;
    let idrs = capture.count("idr");
    beside.say("REQUEST_KEYFRAME").await;
    assert_eq!(
        capture.count("idr"),
        idrs,
        "one request per display per 250 ms"
    );
    capture.deliver(3, 2, false);
    settle().await;
    let pages = rig
        .service
        .reg
        .lock()
        .common
        .get(&(PRIMARY_DISPLAY.to_string(), true))
        .map(|common| common.pages());
    assert_eq!(
        pages,
        Some(2),
        "both pages hold the frames they were sent since the key frame"
    );
}

#[tokio::test]
async fn every_key_frame_request_is_counted_by_its_cause() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    let mut viewer = rig.viewer().await;
    settle().await;
    let capture = rig.primary_capture();
    let metrics = &rig.metrics;
    let pages = metrics.keyframe_requests(PRIMARY_DISPLAY, "page");
    rig.service
        .schedule_idr_for_page(owner.id(), PRIMARY_DISPLAY);
    assert_eq!(
        metrics.keyframe_requests(PRIMARY_DISPLAY, "page"),
        pages + 1,
        "a page starting on the display decodes from one"
    );
    assert_eq!(
        metrics.all_keyframe_requests(PRIMARY_DISPLAY),
        capture.count("idr"),
        "every request that reached the encoder is counted, and nothing else"
    );
    owner.say("REQUEST_KEYFRAME").await;
    owner.say("REQUEST_KEYFRAME").await;
    assert_eq!(
        metrics.keyframe_requests(PRIMARY_DISPLAY, "client"),
        1,
        "a request the floor thins is not counted"
    );
    advance(0.3);
    viewer.say("REQUEST_KEYFRAME").await;
    rig.service
        .schedule_idr(PRIMARY_DISPLAY, KeyFrameCause::FanoutLag);
    rig.service
        .schedule_idr(PRIMARY_DISPLAY, KeyFrameCause::Restart);
    assert_eq!(
        (
            metrics.keyframe_requests(PRIMARY_DISPLAY, "client"),
            metrics.keyframe_requests(PRIMARY_DISPLAY, "fanout_lag"),
            metrics.keyframe_requests(PRIMARY_DISPLAY, "restart"),
        ),
        (2, 1, 1)
    );
    assert_eq!(
        metrics.all_keyframe_requests(PRIMARY_DISPLAY),
        capture.count("idr")
    );
    reset_clock();
}

#[test]
fn the_invalidation_line_names_who_lost_the_frame_and_not_the_encoders_answer() {
    assert_eq!(
        crate::ws::server::invalidation_line(PRIMARY_DISPLAY, 71, false, 0),
        "Display 'primary': frame 71 lost by a client; the encoder stops predicting from it."
    );
    assert_eq!(
        crate::ws::server::invalidation_line(PRIMARY_DISPLAY, 72, true, 3),
        "Display 'primary': frame 72 left out by a relay; the encoder stops predicting from it. \
         (+3 more in the last 5 s)"
    );
}

#[tokio::test]
async fn a_relays_invalidations_are_logged_apart_from_a_clients() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    let logged = |rig: &Rig, withheld: bool| {
        rig.service
            .reg
            .lock()
            .invalidation_log
            .get(&(PRIMARY_DISPLAY.to_string(), withheld))
            .map(|e| e.1)
    };
    rig.service
        .schedule_invalidation(PRIMARY_DISPLAY, 70, false);
    rig.service.schedule_invalidation(PRIMARY_DISPLAY, 71, true);
    assert_eq!(
        (logged(&rig, false), logged(&rig, true)),
        (Some(0), Some(0)),
        "a relay's first report within five seconds of a client's has a line of its own"
    );
    rig.service
        .schedule_invalidation(PRIMARY_DISPLAY, 72, false);
    assert_eq!(
        (logged(&rig, false), logged(&rig, true)),
        (Some(1), Some(0)),
        "a second report of the same cause is counted into the next line"
    );
    advance(5.0);
    rig.service
        .schedule_invalidation(PRIMARY_DISPLAY, 73, false);
    assert_eq!(logged(&rig, false), Some(0), "and logged five seconds on");
    reset_clock();
}

#[tokio::test]
async fn a_rate_change_during_a_layout_pass_is_the_refresh_the_display_keeps() {
    let rig = Rig::new();
    let _page = rig.owner("a").await;
    settle().await;
    rig.service.handle_opcode_fps(144.0, PRIMARY_DISPLAY).await;
    assert_eq!(*rig.display.refresh.lock(), Some(144.0));
    // A pass that read 144 fps is setting the modes of a new size when the rate changes.
    let layouts = |rig: &Rig| {
        let calls = rig.display.calls.lock();
        calls.iter().filter(|c| c.starts_with("layout")).count()
    };
    let before = layouts(&rig);
    let hold = Arc::new(tokio::sync::Notify::new());
    *rig.display.hold_layout.lock() = Some(hold.clone());
    let service = rig.service.clone();
    let pass =
        tokio::spawn(async move { service.handle_resize("1600x900", PRIMARY_DISPLAY).await });
    while layouts(&rig) == before {
        settle().await;
    }
    rig.service.handle_opcode_fps(90.0, PRIMARY_DISPLAY).await;
    assert!(
        rig.primary_capture().has("framerate 90"),
        "the capture takes the rate without waiting for the pass"
    );
    hold.notify_one();
    pass.await.unwrap();
    for _ in 0..100 {
        if *rig.display.refresh.lock() == Some(90.0) {
            break;
        }
        settle().await;
    }
    assert_eq!(
        *rig.display.refresh.lock(),
        Some(90.0),
        "the pass's mode-set cannot leave the display at the rate it read before the change"
    );
}

#[tokio::test]
async fn a_capture_still_starting_takes_the_live_requests() {
    let rig = Rig::new();
    let mut page = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    // The display's capture mid-start: its module takes requests, its settings are registered after.
    {
        let cs = rig
            .service
            .st
            .lock()
            .instances
            .remove(PRIMARY_DISPLAY)
            .map(|i| i.settings)
            .unwrap();
        rig.service
            .st
            .lock()
            .starting
            .insert(PRIMARY_DISPLAY.to_string(), cs);
        rig.service.reg.lock().running.remove(PRIMARY_DISPLAY);
    }
    rig.service
        .handle_opcode_video_bitrate(4000.0, PRIMARY_DISPLAY)
        .await;
    assert!(
        capture.has("bitrate 4000"),
        "a bitrate set while the capture starts reaches its encoder"
    );
    assert_eq!(
        rig.service.st.lock().starting[PRIMARY_DISPLAY].video_bitrate_kbps,
        4000,
        "and is what it is registered with"
    );
    rig.service.handle_opcode_fps(30.0, PRIMARY_DISPLAY).await;
    assert!(capture.has("framerate 30"));
    assert_eq!(
        rig.service.st.lock().starting[PRIMARY_DISPLAY].target_fps,
        30.0
    );
    let idrs = capture.count("idr");
    rig.service
        .schedule_idr(PRIMARY_DISPLAY, KeyFrameCause::Restart);
    assert_eq!(capture.count("idr"), idrs + 1);
    rig.service
        .schedule_invalidation(PRIMARY_DISPLAY, 70000, false);
    assert!(capture.has(&format!("invalidate {} false", 70000 & 0xFFFF)));
    assert!(rig.service.relay_lost(page.id(), PRIMARY_DISPLAY, 71, true));
    assert!(
        capture.has("invalidate 71 true"),
        "a frame a relay left out reaches it as withheld"
    );
    {
        rig.service.reg.lock().captures.remove(PRIMARY_DISPLAY);
        rig.service.st.lock().starting.clear();
    }
    let calls = capture.calls.lock().len();
    rig.service
        .handle_opcode_video_bitrate(3000.0, PRIMARY_DISPLAY)
        .await;
    assert_eq!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].video_bitrate,
        3000.0,
        "with no capture the rate is stored"
    );
    assert_eq!(capture.calls.lock().len(), calls, "and nothing is called");
    page.say("_stats,0").await;
}

#[tokio::test]
async fn the_decode_pace_holds_the_capture_and_never_outlives_its_page() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    let mut viewer = rig.viewer().await;
    settle().await;
    let capture = rig.primary_capture();
    owner.say(r#"SETTINGS,{"framerate":60}"#).await;
    settle().await;
    viewer.say("DECODE_PACE 41").await;
    assert!(
        !capture.has("framerate 41"),
        "a viewer's is its own gate's to answer"
    );
    owner.say("DECODE_PACE 41").await;
    assert!(capture.has("framerate 41"));
    assert_eq!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].framerate,
        60.0,
        "the chosen rate stays stored"
    );
    let n = capture.count("framerate");
    owner.say("DECODE_PACE 41").await;
    assert_eq!(
        capture.count("framerate"),
        n,
        "the same pace again changes nothing"
    );
    owner.say("DECODE_PACE 0").await;
    assert!(
        capture.has("framerate 60"),
        "lifting the pace puts the chosen rate back"
    );
    owner.say("DECODE_PACE 36").await;
    let mut successor = rig.controller().await;
    successor
        .say(r#"SETTINGS,{"tabId":"a","initialClientWidth":1280,"initialClientHeight":720}"#)
        .await;
    settle().await;
    assert_eq!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].decode_pace,
        None,
        "a new connection starts unpaced"
    );
    assert!(
        owner
            .has("KILL a new primary client connected connection killed")
            .await
    );
}

#[tokio::test]
async fn the_same_tab_takes_the_display_over_warm() {
    let rig = Rig::new();
    let owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    let starts = capture.count("start");
    let idrs = capture.count("idr");
    let mut next = rig.controller().await;
    next.say(r#"SETTINGS,{"tabId":"a","initialClientWidth":1280,"initialClientHeight":720,"encoder":"h264enc"}"#).await;
    settle().await;
    assert!(
        owner
            .has("KILL a new primary client connected connection killed")
            .await
    );
    assert!(owner.client().closed(), "closed 1000");
    assert!(rig.service.reg.lock().owns(next.id(), PRIMARY_DISPLAY));
    assert!(next.has("PIPELINE_RESETTING primary").await);
    assert_eq!(capture.count("start"), starts, "the capture is kept");
    assert!(capture.count("idr") > idrs);
    assert_eq!(
        rig.service
            .link(PRIMARY_DISPLAY)
            .unwrap()
            .lock()
            .acknowledged_frame_id,
        -1,
        "its acks start over"
    );
}

#[tokio::test]
async fn another_tab_joins_beside_the_owner() {
    let rig = Rig::new();
    let owner = rig.owner("a").await;
    settle().await;
    let mut beside = rig.controller().await;
    beside
        .say(r#"SETTINGS,{"tabId":"b","initialClientWidth":640,"initialClientHeight":480}"#)
        .await;
    settle().await;
    assert!(rig.service.reg.lock().is_beside(beside.id()));
    assert!(
        rig.service.reg.lock().owns(owner.id(), PRIMARY_DISPLAY),
        "the owner keeps the display"
    );
    assert_eq!(
        rig.layout(PRIMARY_DISPLAY).map(|l| l.w),
        Some(1280),
        "and its size"
    );
    let texts = beside.texts().await;
    assert!(texts.iter().any(|t| t == "PIPELINE_RESETTING primary"));
    assert_eq!(
        typed(&texts, "display_settings").expect("told what it streams with")["settings"]["encoder"],
        json!("h264enc")
    );
    assert_eq!(
        typed(&texts, "stream_resolution").unwrap()["width"],
        json!(1280)
    );
    beside.say("r,640x480").await;
    beside.say("s,144").await;
    assert!(
        rig.input.messages.lock().is_empty(),
        "the owner sizes the display and sets its density"
    );
    beside.say("kd,65").await;
    assert_eq!(
        rig.input.messages.lock()[0].0,
        "kd,65",
        "its input is a controller's"
    );

    // What it changed on its own follows the owner, and it is told so.
    beside.clear().await;
    beside
        .say(r#"SETTINGS,{"tabId":"b","encoder":"vp8enc","framerate":144}"#)
        .await;
    settle().await;
    assert_eq!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].encoder,
        "h264enc"
    );
    assert!(typed(&beside.texts().await, "display_settings").is_some());

    // A pick of the display's size or density is the owner's.
    beside.say(r#"SETTINGS,{"tabId":"b","scaling_dpi":144,"picked":["scaling_dpi","manual_resolution"]}"#).await;
    assert!(rig.display.calls.lock().iter().all(|c| c != "dpi 144"));

    // What its user picked applies, with nothing it did not pick; both pages are told.
    owner.clear().await;
    beside.say(r#"SETTINGS,{"tabId":"b","encoder":"h264enc","video_crf":18,"manual_resolution":true,"picked":["video_crf"]}"#).await;
    settle().await;
    let st_crf = rig.service.st.lock().displays[PRIMARY_DISPLAY].video_crf;
    assert_eq!(st_crf, 18);
    assert_eq!(
        rig.layout(PRIMARY_DISPLAY).map(|l| l.w),
        Some(1280),
        "with nothing it did not pick, the display's geometry among it"
    );
    assert_eq!(
        typed(&owner.texts().await, "display_settings").expect("the owner is told")["settings"]["video_crf"],
        json!(18)
    );
    assert!(rig.primary_capture().has("tunables"), "a crf applies live");
}

/// A page joining beside the owner is sent one key frame: its relay asks for one at the first
/// delta it drops, and the join asks only where the relay has not.
#[tokio::test]
async fn a_page_joining_beside_is_asked_one_key_frame() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    capture.deliver(1, 1, true);
    settle().await;
    let mut beside = rig.controller().await;
    settle().await;
    let idrs = capture.count("idr");
    // The page's relay, made at the first frame since it connected, drops the delta and asks.
    capture.deliver(2, 1, false);
    settle().await;
    assert_eq!(
        capture.count("idr"),
        idrs + 1,
        "the relay asks for the page's key frame"
    );
    beside
        .say(r#"SETTINGS,{"tabId":"b","initialClientWidth":640,"initialClientHeight":480}"#)
        .await;
    settle().await;
    assert!(rig.service.reg.lock().is_beside(beside.id()));
    assert_eq!(
        capture.count("idr"),
        idrs + 1,
        "the join adds no second one"
    );
}

#[tokio::test]
async fn a_pick_needs_full_permissions_on_both_pages() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    let mut beside = rig.controller().await;
    beside.say(r#"SETTINGS,{"tabId":"b"}"#).await;
    *rig.tokens.mk.lock() = Some("someone".into());
    beside
        .say(r#"SETTINGS,{"tabId":"b","video_crf":40,"picked":["video_crf"]}"#)
        .await;
    assert_ne!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].video_crf,
        40,
        "another token holds keyboard and mouse"
    );
    *rig.tokens.mk.lock() = None;
}

#[tokio::test]
async fn a_hidden_owner_hands_the_display_to_the_page_that_shows() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let mut beside = rig.controller().await;
    beside.say(r#"SETTINGS,{"tabId":"b"}"#).await;
    owner.say("PAGE_HIDDEN").await;
    settle().await;
    assert!(
        rig.service.reg.lock().owns(owner.id(), PRIMARY_DISPLAY),
        "beside a page that never said it shows"
    );
    beside.say("PAGE_SHOWN").await;
    assert!(
        rig.service.reg.lock().owns(owner.id(), PRIMARY_DISPLAY),
        "an owner just hidden keeps it a moment"
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(rig.service.reg.lock().owns(beside.id(), PRIMARY_DISPLAY));
    assert!(beside.has("DISPLAY_OWNER primary").await);
    assert!(
        rig.service.reg.lock().is_beside(owner.id()),
        "the owner stays beside it"
    );
    assert!(
        rig.service.reg.lock().video_paused.contains(&owner.id()),
        "paused while hidden"
    );
    assert_eq!(
        rig.service
            .link(PRIMARY_DISPLAY)
            .unwrap()
            .lock()
            .rtt_floor_ms,
        None,
        "the link state starts over"
    );
}

#[tokio::test]
async fn an_idle_owner_hands_the_display_to_the_page_in_use() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let mut beside = rig.controller().await;
    beside.say(r#"SETTINGS,{"tabId":"b"}"#).await;
    beside.say("PAGE_SHOWN").await;
    beside.say("m,1,1,0,0").await;
    assert!(
        rig.service.reg.lock().owns(owner.id(), PRIMARY_DISPLAY),
        "an idle owner that never said it shows keeps it"
    );
    owner.say("PAGE_SHOWN").await;
    beside.say("m,2,2,0,0").await;
    assert!(
        rig.service.reg.lock().owns(owner.id(), PRIMARY_DISPLAY),
        "an owner just seated is in use"
    );
    advance(4.0);
    beside.say("ku,65").await;
    assert!(
        rig.service.reg.lock().owns(owner.id(), PRIMARY_DISPLAY),
        "a key's release is no use of a page"
    );
    beside.say("js,a,0,1,0.2").await;
    assert!(
        rig.service.reg.lock().owns(owner.id(), PRIMARY_DISPLAY),
        "nor a gamepad's axis"
    );
    beside.say("m,3,3,1,0").await;
    assert!(
        rig.service.reg.lock().owns(beside.id(), PRIMARY_DISPLAY),
        "an owner gone unused hands the display to the page in use"
    );
    assert!(
        !rig.service.reg.lock().video_paused.contains(&owner.id()),
        "the owner, in view, stays beside it and streams"
    );
    assert!(
        rig.input.messages.lock().iter().any(|m| m.0 == "m,3,3,1,0"),
        "its input taken first"
    );
    reset_clock();
}

#[tokio::test]
async fn audio_is_the_owners_for_itself_and_the_viewers_and_each_beside_pages_own() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let mut beside = rig.controller().await;
    beside.say(r#"SETTINGS,{"tabId":"b"}"#).await;
    let viewer = rig.viewer().await;
    owner.clear().await;
    beside.clear().await;
    viewer.clear().await;
    owner.say("START_AUDIO").await;
    settle().await;
    assert!(owner.has("AUDIO_STARTED").await && viewer.has("AUDIO_STARTED").await);
    assert!(
        !beside.has("AUDIO_STARTED").await,
        "the owner's start is not the page beside it"
    );
    assert_eq!(rig.service.reg.lock().session_audio, Some(true));
    assert!(!rig.service.reg.lock().hears_audio(beside.id()));
    beside.say("START_AUDIO").await;
    settle().await;
    assert!(beside.has("AUDIO_STARTED").await);
    assert!(rig.service.reg.lock().hears_audio(beside.id()));
    owner.say("STOP_AUDIO").await;
    settle().await;
    assert!(
        rig.service.audio_wanted(),
        "the capture runs for the page beside it"
    );
    assert!(*rig.audio.capturing.lock());
    beside.say("STOP_AUDIO").await;
    settle().await;
    assert!(!*rig.audio.capturing.lock(), "nobody hearing, it stops");
    assert!(beside.has("AUDIO_STOPPED").await);
}

#[tokio::test]
async fn the_reconnect_grace_holds_the_display_then_tears_it_down() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    owner.client().set_closed(Some(1006));
    owner.conn.teardown().await;
    assert_eq!(
        *rig.input.released.lock(),
        0,
        "a cut leaves the keys held through the grace"
    );
    assert!(
        rig.service.st.lock().displays.contains_key(PRIMARY_DISPLAY),
        "the entry waits"
    );
    assert!(
        rig.primary_capture().is_capturing_now(),
        "and its capture runs warm"
    );
    assert_eq!(
        rig.input.gamepads.lock().clone(),
        vec![owner.id()],
        "gamepads go at once"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        *rig.input.released.lock(),
        1,
        "released once the page did not come back"
    );
    assert!(!rig.service.st.lock().displays.contains_key(PRIMARY_DISPLAY));
    assert!(!rig.primary_capture().is_capturing_now());
}

#[tokio::test]
async fn a_page_back_within_the_grace_claims_the_display_warm() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let starts = rig.primary_capture().count("start");
    owner.client().set_closed(Some(1000));
    owner.conn.teardown().await;
    assert_eq!(
        *rig.input.released.lock(),
        1,
        "a clean close lets go at once"
    );
    let mut back = rig.controller().await;
    back.say(r#"SETTINGS,{"tabId":"a","initialClientWidth":1280,"initialClientHeight":720,"encoder":"h264enc"}"#).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(rig.service.reg.lock().owns(back.id(), PRIMARY_DISPLAY));
    assert!(
        rig.service.st.lock().displays.contains_key(PRIMARY_DISPLAY),
        "claimed: teardown skipped"
    );
    assert_eq!(
        rig.primary_capture().count("start"),
        starts,
        "on the capture it left running"
    );
}

#[tokio::test]
async fn an_owner_gone_for_good_promotes_the_page_beside_it() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let mut first = rig.controller().await;
    first.say(r#"SETTINGS,{"tabId":"b"}"#).await;
    let mut second = rig.controller().await;
    second.say(r#"SETTINGS,{"tabId":"c"}"#).await;
    *rig.tokens.mk.lock() = Some("c".into());
    let mut perms = second.client().perms();
    perms.token = Some("c".into());
    second.client().set_perms(perms);
    owner.client().set_closed(Some(1000));
    owner.conn.teardown().await;
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert!(
        second.has("DISPLAY_OWNER primary").await,
        "the oldest with full permissions"
    );
    assert!(!first.has("DISPLAY_OWNER primary").await);
    second.say(r#"SETTINGS,{"tabId":"c"}"#).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        rig.service.reg.lock().owns(second.id(), PRIMARY_DISPLAY),
        "its settings claim the entry"
    );
    assert!(
        !first.has("DISPLAY_OWNER primary").await,
        "and no one else is promoted"
    );
    assert!(
        rig.service.reg.lock().is_beside(first.id()),
        "the other stays beside it"
    );
    *rig.tokens.mk.lock() = None;
}

#[tokio::test]
async fn a_token_table_change_reaches_live_sockets() {
    let rig = Rig::new();
    let table = |entries: Vec<(&str, TokenEntry)>| {
        Some(
            entries
                .into_iter()
                .map(|(t, e)| (t.to_string(), e))
                .collect::<Vec<_>>(),
        )
    };
    *rig.tokens.table.lock() = table(vec![
        ("gone", entry("controller", Json::Null)),
        ("role", entry("controller", Json::Null)),
        ("slot", entry("viewer", json!([1, 2]))),
    ]);
    rig.service.st.lock().last_connections.clear();
    let gone = rig.connect("gone", "", None).await.unwrap();
    rig.service.st.lock().last_connections.clear();
    let role = rig.connect("role", "", None).await.unwrap();
    rig.service.st.lock().last_connections.clear();
    let slot = rig.connect("slot", "", None).await.unwrap();
    slot.clear().await;
    *rig.tokens.table.lock() = table(vec![
        ("role", entry("viewer", Json::Null)),
        ("slot", entry("viewer", json!(2))),
    ]);
    rig.service.reconcile_clients().await;
    assert_eq!(gone.client().close_code(), Some(4002), "token revoked");
    assert_eq!(
        role.client().close_code(),
        Some(4002),
        "permissions changed significantly"
    );
    let texts = slot.texts().await;
    assert_eq!(texts[0], r#"ROLE_UPDATE,{"role":"viewer","slot":2}"#);
    assert_eq!(texts[1], "MK_ACCESS,0");
    assert_eq!(slot.client().perms().slot, json!(2));
    assert_eq!(
        rig.input.lost_slots.lock().clone(),
        vec![(slot.id(), vec![1])],
        "the lost slot is released"
    );
}

#[tokio::test]
async fn secure_input_needs_authority() {
    let rig = Rig::new();
    *rig.tokens.table.lock() = Some(vec![
        ("a".into(), entry("controller", Json::Null)),
        ("b".into(), entry("controller", Json::Null)),
    ]);
    *rig.tokens.mk.lock() = Some("a".into());
    rig.service.st.lock().last_connections.clear();
    let mut holder = rig.connect("a", "", None).await.unwrap();
    rig.service.st.lock().last_connections.clear();
    let mut outranked = rig.connect("b", "", None).await.unwrap();
    assert!(outranked.has("MK_ACCESS,0").await);
    outranked.say("kd,65").await;
    outranked.say("cmd,xterm").await;
    assert!(rig.input.messages.lock().is_empty());
    outranked.say("vb,2000").await;
    assert_eq!(
        rig.input.messages.lock().len(),
        1,
        "what is not input still reaches the dialect"
    );
    holder.say("kd,65").await;
    assert_eq!(rig.input.messages.lock().len(), 2);
}

#[tokio::test]
async fn stats_go_only_to_a_page_that_asked() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    let mut viewer = rig.viewer().await;
    settle().await;
    owner.clear().await;
    viewer.clear().await;
    rig.service.send_stream_stats();
    settle().await;
    assert!(
        typed(&owner.texts().await, "stream_stats").is_none(),
        "nothing periodic while nobody looks"
    );
    owner.say("_stats,1").await;
    viewer.say("_stats,1").await;
    rig.service.send_stream_stats();
    settle().await;
    let stats = typed(&owner.texts().await, "stream_stats").expect("the controller that asked");
    assert_eq!(stats["displayId"], json!("primary"));
    assert_eq!(stats["stats"]["cpu_percent"], json!(12.5));
    assert_eq!(stats["stats"]["throttled"], json!(false));
    assert!(stats["stats"].get("rtt_ms").is_some());
    assert!(
        typed(&viewer.texts().await, "stream_stats").is_none(),
        "a shared viewer is sent neither"
    );
    owner.clear().await;
    owner.say("_stats,0").await;
    rig.service.send_stream_stats();
    settle().await;
    assert!(typed(&owner.texts().await, "stream_stats").is_none());
    rig.service
        .publish_stream_info(PRIMARY_DISPLAY, Default::default())
        .await;
    assert!(typed(&owner.texts().await, "stream_info").is_some());
}

#[tokio::test]
async fn reconciliation_waits_for_publication_and_leaves_the_send_set_clean() {
    let rig = Rig::new();
    let owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    let actual = settled_encoder("x264", false);
    *capture.info.lock() = Some(actual.clone());
    *capture.info_delay.lock() = Duration::ZERO;
    rig.service
        .st
        .lock()
        .instances
        .get_mut(PRIMARY_DISPLAY)
        .unwrap()
        .settings
        .video_reference_frames = 0;
    let starts = capture.count("start");
    let socket = FakeSocket::new(owner.id(), 0.0, 0, crate::ws::client::Liveness::default());
    socket
        .hold_stream_info
        .store(true, std::sync::atomic::Ordering::Relaxed);
    rig.service
        .reg
        .lock()
        .clients
        .insert(owner.id(), socket.client.clone());
    let service = rig.service.clone();
    let socket_client = socket.client.clone();
    let publication = tokio::spawn(async move {
        service
            .send_stream_message(
                socket_client,
                json!({"type": "stream_info", "displayId": PRIMARY_DISPLAY}),
            )
            .await;
    });
    for _ in 0..20 {
        if rig.service.reg.lock().stats_sending.contains(&owner.id()) {
            break;
        }
        settle().await;
    }
    assert!(rig.service.reg.lock().stats_sending.contains(&owner.id()));
    publication.abort();
    let _ = publication.await;
    socket
        .hold_stream_info
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(
        !rig.service.reg.lock().stats_sending.contains(&owner.id()),
        "cancelling an in-flight publication releases its send-set membership"
    );

    rig.service
        .publish_stream_info(PRIMARY_DISPLAY, crate::stats::stream_info_message(&actual))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(capture.count("start"), starts + 1);
    let writes = socket.writes.lock().len();
    rig.service
        .send_stream_message(
            socket.client.clone(),
            json!({"type": "stream_info", "probe": true}),
        )
        .await;
    assert!(socket.writes.lock().len() > writes);
    socket.stop();
}

#[tokio::test]
async fn a_stream_box_is_rebroadcast_with_the_layout() {
    let rig = Rig::new();
    let owner = rig.owner("a").await;
    settle().await;
    owner.clear().await;
    rig.service
        .set_client_stream_box("nowhere", 1.0, 2.0, 1.0, 1.0)
        .await;
    rig.service
        .set_client_stream_box(PRIMARY_DISPLAY, 1.0, 2.0, 0.01, 1.0)
        .await;
    rig.service
        .set_client_stream_box(PRIMARY_DISPLAY, 1.0e6, 2.0, 1.0, 1.0)
        .await;
    assert!(
        owner.texts().await.is_empty(),
        "an unknown display or an impossible box is ignored"
    );
    rig.service
        .set_client_stream_box(PRIMARY_DISPLAY, 10.0, 20.0, 1.5, 2.0)
        .await;
    let texts = owner.texts().await;
    let payload: Json =
        serde_json::from_str(texts[0].strip_prefix("DISPLAY_CONFIG_UPDATE,").unwrap()).unwrap();
    let layout = &payload["layouts"]["primary"];
    assert_eq!(
        (&layout["originX"], &layout["originY"]),
        (&json!(10.0), &json!(20.0))
    );
    assert_eq!(
        (&layout["scaleX"], &layout["scaleY"]),
        (&json!(1.5), &json!(2.0))
    );
    owner.clear().await;
    rig.service
        .set_client_stream_box(PRIMARY_DISPLAY, 11.0, 20.0, 1.5, 2.0)
        .await;
    assert!(owner.texts().await.is_empty(), "floored at 0.2 s");
    advance(0.3);
    rig.service
        .set_client_stream_box(PRIMARY_DISPLAY, 10.0, 20.0, 1.5, 2.0)
        .await;
    assert!(owner.texts().await.is_empty(), "the same box is no change");
    reset_clock();
}

#[tokio::test]
async fn a_page_payload_carries_what_its_display_streams_with() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    owner.say(r#"SETTINGS,{"tabId":"a","framerate":24,"video_crf":33,"rate_control_mode":"crf","encoder":"jpeg"}"#).await;
    settle().await;
    let payload = rig.service.settings_payload_for_display(PRIMARY_DISPLAY);
    assert_eq!(payload["framerate"]["default"], json!(24));
    assert_eq!(payload["rate_control_mode"]["value"], json!("crf"));
    assert_eq!(payload["video_crf"]["default"], json!(33));
    assert_eq!(payload["encoder"]["value"], json!("jpeg"));
    let session = rig.service.st.lock().seeds.encoder.clone();
    let other = rig.service.settings_payload_for_display("display2");
    assert_eq!(
        other["encoder"]["value"],
        json!(session),
        "a display holding no picks gets the session encoder"
    );
}

#[tokio::test]
async fn a_structural_switch_restarts_the_capture_and_re_announces_it() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    let viewer = rig.viewer().await;
    settle().await;
    let capture = rig.primary_capture();
    let starts = capture.count("start");
    owner.clear().await;
    viewer.clear().await;
    owner
        .say(r#"SETTINGS,{"tabId":"a","encoder":"vp8enc"}"#)
        .await;
    settle().await;
    assert_eq!(capture.count("start"), starts + 1);
    assert_eq!(capture.count("stop"), 1);
    assert_eq!(
        typed(&viewer.texts().await, "server_settings").expect("viewers re-key their demux")["settings"]
            ["encoder"]["value"],
        json!("vp8enc")
    );
    assert!(owner.has("PIPELINE_RESETTING primary").await);
    assert!(
        !rig.service.st.lock().displays[PRIMARY_DISPLAY].cpu_encode_threading,
        "unset, a vp8enc session runs one thread"
    );
    let tunables = capture.count("tunables");
    owner
        .say(r#"SETTINGS,{"tabId":"a","encoder":"vp8enc","video_crf":21}"#)
        .await;
    settle().await;
    assert_eq!(
        capture.count("start"),
        starts + 1,
        "a repeated encoder is no switch"
    );
    assert_eq!(
        capture.count("tunables"),
        tunables + 1,
        "and a tunable applies live"
    );
}

#[tokio::test]
async fn software_encoding_does_not_outlive_a_cpu_only_encoder() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let use_cpu = |rig: &Rig| rig.service.st.lock().displays[PRIMARY_DISPLAY].use_cpu;
    assert!(!use_cpu(&rig));
    owner
        .say(r#"SETTINGS,{"tabId":"a","encoder":"jpeg"}"#)
        .await;
    settle().await;
    assert!(use_cpu(&rig), "jpeg switches to software");
    owner
        .say(r#"SETTINGS,{"tabId":"a","encoder":"h264enc"}"#)
        .await;
    settle().await;
    assert!(!use_cpu(&rig), "returning to h264enc restores hardware");
}

#[tokio::test]
async fn a_demoted_codec_reaches_the_pages() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    owner
        .say(r#"SETTINGS,{"tabId":"a","encoder":"av1enc"}"#)
        .await;
    settle().await;
    owner.clear().await;
    *rig.primary_capture().active.lock() = None;
    rig.service.settle_active_codec(PRIMARY_DISPLAY, 9).await;
    assert_eq!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].encoder,
        "av1enc",
        "an undecided capture changes nothing"
    );
    *rig.primary_capture().active.lock() = Some("av1".into());
    rig.service.settle_active_codec(PRIMARY_DISPLAY, 0).await;
    assert_eq!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].encoder,
        "av1enc",
        "the requested codec changes nothing"
    );
    assert!(typed(&owner.texts().await, "server_settings").is_none());
    *rig.primary_capture().active.lock() = Some("h264".into());
    rig.service.settle_active_codec(PRIMARY_DISPLAY, 0).await;
    assert_eq!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].encoder,
        "h264enc"
    );
    assert_eq!(
        typed(&owner.texts().await, "server_settings").unwrap()["settings"]["encoder"]["value"],
        json!("h264enc")
    );
}

fn settled_encoder(name: &str, hardware: bool) -> pixelflux::StreamInfo {
    pixelflux::StreamInfo {
        info: pixelflux::report::StreamInfo {
            encoder: name.into(),
            hardware,
            codec: "h264",
            ..Default::default()
        },
        renderer: None,
    }
}

#[tokio::test]
async fn an_nvenc_planned_capture_that_settles_on_software_restores_one_reference() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    {
        let mut st = rig.service.st.lock();
        st.displays.get_mut(PRIMARY_DISPLAY).unwrap().framerate = 8.0;
        let settings = &mut st.instances.get_mut(PRIMARY_DISPLAY).unwrap().settings;
        settings.video_reference_frames = 0;
        settings.target_fps = 8.0;
    }
    *capture.reselect_backend.lock() = true;
    let nvenc = settled_encoder("NVENC", true);
    *capture.info.lock() = Some(nvenc.clone());
    let starts = capture.count("start");
    rig.service
        .publish_stream_info(PRIMARY_DISPLAY, crate::stats::stream_info_message(&nvenc))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(capture.count("start"), starts);

    // Mid-stream failure demotion reports the same actual software encoder as startup fallback.
    let actual = settled_encoder("x264", false);
    *capture.info.lock() = Some(actual.clone());
    rig.service
        .publish_stream_info(PRIMARY_DISPLAY, crate::stats::stream_info_message(&actual))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    {
        let starts_now = capture.starts.lock();
        assert_eq!(
            starts_now.len(),
            starts + 1,
            "the actual software session is reopened with the WebSocket cap"
        );
        assert_eq!(starts_now.last().unwrap().video_reference_frames, 1);
        assert_eq!(starts_now.last().unwrap().encode_node_index, -1);
        assert_eq!(starts_now.last().unwrap().target_fps, 8.0);
        assert_eq!(
            capture.info.lock().as_ref().unwrap().info.encoder,
            "x264",
            "the corrected reopen stays on the actual software backend"
        );
        assert_eq!(
            rig.service.st.lock().instances[PRIMARY_DISPLAY]
                .settings
                .video_reference_frames,
            1
        );
        assert_eq!(
            rig.service
                .capture_settings(PRIMARY_DISPLAY, rig.layout(PRIMARY_DISPLAY).unwrap())
                .unwrap()
                .video_reference_frames,
            1,
            "a later live settings refresh does not reapply the startup hint"
        );
        assert_eq!(
            rig.service
                .capture_settings(PRIMARY_DISPLAY, rig.layout(PRIMARY_DISPLAY).unwrap())
                .unwrap()
                .encode_node_index,
            -1,
            "later settings retain the software demotion"
        );
    }
    rig.service
        .publish_stream_info(PRIMARY_DISPLAY, crate::stats::stream_info_message(&actual))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        capture.count("start"),
        starts + 1,
        "the software report does not start an NVENC/software restart cycle"
    );
}

#[tokio::test]
async fn an_actual_nvenc_session_keeps_its_reference_window_without_a_restart() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    rig.service
        .st
        .lock()
        .instances
        .get_mut(PRIMARY_DISPLAY)
        .unwrap()
        .settings
        .video_reference_frames = 0;
    let actual = settled_encoder("NVENC", true);
    *capture.info.lock() = Some(actual.clone());
    let starts = capture.count("start");

    rig.service
        .publish_stream_info(PRIMARY_DISPLAY, crate::stats::stream_info_message(&actual))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(capture.count("start"), starts);
    assert_eq!(
        rig.service.st.lock().instances[PRIMARY_DISPLAY]
            .settings
            .video_reference_frames,
        0
    );
}

#[tokio::test]
async fn actual_non_nvenc_sessions_keep_their_existing_reference_cap() {
    for (encoder, hardware) in [
        ("x264", false),
        ("VAAPI", true),
        ("V4L2M2M", true),
        ("TEGRA", true),
    ] {
        let rig = Rig::new();
        let _owner = rig.owner("a").await;
        settle().await;
        let capture = rig.primary_capture();
        assert_eq!(
            rig.service.st.lock().instances[PRIMARY_DISPLAY]
                .settings
                .video_reference_frames,
            1
        );
        if !hardware {
            rig.service
                .st
                .lock()
                .instances
                .get_mut(PRIMARY_DISPLAY)
                .unwrap()
                .settings
                .encode_node_index = -1;
        }
        let actual = settled_encoder(encoder, hardware);
        *capture.info.lock() = Some(actual.clone());
        let starts = capture.count("start");

        rig.service
            .publish_stream_info(PRIMARY_DISPLAY, crate::stats::stream_info_message(&actual))
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(capture.count("start"), starts, "{encoder}");
        assert_eq!(
            rig.service.st.lock().instances[PRIMARY_DISPLAY]
                .settings
                .video_reference_frames,
            1,
            "{encoder}"
        );
    }
}

/// A second page on a display whose encoder keeps one reference has the encoder take its own
/// depth in place, as a live tunable with no capture restart. The depth stays once the display
/// is down to one page, through a reconcile, until the capture next starts.
#[tokio::test]
async fn a_second_page_takes_the_encoders_own_reference_depth() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    let frames = |rig: &Rig| {
        rig.service.st.lock().instances[PRIMARY_DISPLAY]
            .settings
            .video_reference_frames
    };
    rig.service
        .st
        .lock()
        .instances
        .get_mut(PRIMARY_DISPLAY)
        .unwrap()
        .settings
        .encode_node_index = -1;
    assert_eq!(frames(&rig), 1, "one page, one reference");
    let (starts, tunables) = (capture.count("start"), capture.count("tunables"));

    let mut viewer = rig.viewer().await;
    viewer.say("SETTINGS,{}").await;
    settle().await;
    assert_eq!(frames(&rig), 0, "two pages, the encoder's own depth");
    assert!(capture.count("tunables") > tunables, "taken live");
    assert_eq!(capture.count("start"), starts, "with no restart");

    viewer.say("STOP_VIDEO").await;
    settle().await;
    let actual = settled_encoder("x264", false);
    *capture.info.lock() = Some(actual.clone());
    rig.service
        .publish_stream_info(PRIMARY_DISPLAY, crate::stats::stream_info_message(&actual))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(frames(&rig), 0, "one page again, the depth stays");
    assert_eq!(
        capture.count("start"),
        starts,
        "and nothing restarts for it"
    );

    let layout = rig.layout(PRIMARY_DISPLAY).unwrap();
    rig.service.stop_capture_for_display(PRIMARY_DISPLAY).await;
    rig.service
        .start_capture_for_display(PRIMARY_DISPLAY, layout)
        .await;
    assert_eq!(
        capture.starts.lock().last().unwrap().video_reference_frames,
        1,
        "its next start takes one reference again"
    );
}

/// A page on each of two displays is one page per display: the primary keeps one reference.
#[tokio::test]
async fn a_page_on_another_display_leaves_the_primary_one_reference() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    let mut second = rig.controller().await;
    second.say(&secondary_settings("display2", "left")).await;
    settle().await;
    assert!(rig.captures.of("display2").is_some());
    assert_eq!(
        rig.service.st.lock().instances[PRIMARY_DISPLAY]
            .settings
            .video_reference_frames,
        1
    );
    assert!(
        rig.primary_capture()
            .starts
            .lock()
            .iter()
            .all(|s| s.video_reference_frames == 1)
    );
}

#[tokio::test]
async fn the_rate_control_and_quality_verbs_apply() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    owner
        .say(r#"SETTINGS,{"tabId":"a","rate_control_mode":"cbr"}"#)
        .await;
    settle().await;
    let starts = capture.count("start");
    rig.service
        .handle_opcode_rate_control("RateControlMode.CRF", PRIMARY_DISPLAY)
        .await;
    settle().await;
    assert_eq!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].rate_control_mode,
        "crf"
    );
    assert_eq!(
        capture.count("start"),
        starts + 1,
        "structural: the capture restarts"
    );
    rig.service
        .handle_opcode_rate_control("crf", PRIMARY_DISPLAY)
        .await;
    assert_eq!(
        capture.count("start"),
        starts + 1,
        "the mode it has changes nothing"
    );
    let tunables = capture.count("tunables");
    rig.service.handle_opcode_crf(30, PRIMARY_DISPLAY).await;
    assert_eq!(capture.count("tunables"), tunables + 1);
    rig.service.handle_opcode_audio_bitrate(96000.0).await;
    assert_eq!(rig.audio.bitrates.lock().last(), Some(&96000));
    rig.service
        .handle_opcode_video_bitrate(1.0e9, PRIMARY_DISPLAY)
        .await;
    let hi = crate::settings::settings().video_bitrate().1.as_f64();
    assert_eq!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].video_bitrate,
        hi,
        "sanitized like SETTINGS"
    );
    assert!(
        rig.service.st.lock().seeds.video_bitrate == hi,
        "the primary's reseeds the session"
    );
}

#[tokio::test]
async fn native_cursor_rendering_reconfigures_a_running_capture() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    let before = capture.starts.lock().len();
    owner.say("SET_NATIVE_CURSOR_RENDERING,true").await;
    settle().await;
    assert!(rig.service.st.lock().capture_cursor);
    assert!(
        rig.service.st.lock().instances[PRIMARY_DISPLAY]
            .settings
            .capture_cursor,
        "the tracked settings carry it"
    );
    assert_eq!(
        capture.starts.lock().len(),
        before,
        "a tunable, followed live"
    );
    assert!(capture.has("tunables"));
}

#[tokio::test]
async fn a_second_display_extends_the_layout() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    let mut second = rig.controller().await;
    second.say(&secondary_settings("display2", "left")).await;
    settle().await;
    let (p, s) = (
        rig.layout(PRIMARY_DISPLAY).unwrap(),
        rig.layout("display2").unwrap(),
    );
    assert_eq!((s.x, s.w, s.h), (0, 800, 600));
    assert_eq!(p.x, 800, "the primary moves aside");
    assert!(rig.captures.of("display2").unwrap().has("start 800x600"));
    assert!(
        rig.primary_capture().has("region 800 0 1280 720"),
        "a primary kept live is re-targeted"
    );
    let capture = rig.primary_capture();
    capture.deliver(1, 1, true);
    settle().await;
    assert!(
        second.binaries().is_empty(),
        "a secondary's socket leaves the primary fan-out"
    );
    let config = rig.service.display_config_payload();
    assert_eq!(config["displays"], json!(["primary", "display2"]));
    // A partial SETTINGS is no move.
    second
        .say(r#"SETTINGS,{"displayId":"display2","keyboardLayout":"de"}"#)
        .await;
    assert_eq!(rig.layout("display2").unwrap().x, 0);
    assert_eq!(rig.service.st.lock().displays["display2"].position, "left");
}

#[tokio::test]
async fn a_new_secondary_deactivates_the_old_one() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    let mut first = rig.controller().await;
    first.say(&secondary_settings("display2", "right")).await;
    settle().await;
    let mut other = rig.controller().await;
    other.say(&secondary_settings("display3", "right")).await;
    settle().await;
    assert!(first.has("VIDEO_STOPPED").await);
    assert!(!rig.captures.of("display2").unwrap().is_capturing_now());
    assert!(!rig.service.st.lock().displays["display2"].video_active);
}

#[tokio::test]
async fn a_realized_root_smaller_than_the_layout_clamps_and_drops() {
    let rig = Rig::new();
    *rig.display.root.lock() = Some((1000, 700));
    let owner = rig.owner("a").await;
    settle().await;
    assert_eq!(
        rig.layout(PRIMARY_DISPLAY).map(|l| (l.w, l.h)),
        Some((1000, 700)),
        "clamped to the realized root"
    );
    assert_eq!(rig.service.st.lock().displays[PRIMARY_DISPLAY].width, 1000);
    assert_eq!(
        typed(&owner.texts().await, "stream_resolution").unwrap()["width"],
        json!(1000)
    );
    let mut second = rig.controller().await;
    second.say(&secondary_settings("display2", "right")).await;
    settle().await;
    assert!(
        second
            .has("KILL The X server cannot extend the desktop to fit this display.")
            .await
    );
    assert!(!rig.service.st.lock().displays.contains_key("display2"));
}

/// Where the display already fitted the layout to the realized root, as the logical-monitor
/// path does, the pages still hear their clamped sizes and the display with no room is refused.
#[tokio::test]
async fn a_layout_the_display_fitted_in_place_still_clamps_and_drops() {
    let rig = Rig::new();
    *rig.display.root.lock() = Some((1000, 700));
    *rig.display.fits.lock() = true;
    let owner = rig.owner("a").await;
    settle().await;
    assert_eq!(rig.service.st.lock().displays[PRIMARY_DISPLAY].width, 1000);
    assert_eq!(
        typed(&owner.texts().await, "stream_resolution").unwrap()["width"],
        json!(1000)
    );
    let mut second = rig.controller().await;
    second.say(&secondary_settings("display2", "right")).await;
    settle().await;
    assert!(
        second
            .has("KILL The X server cannot extend the desktop to fit this display.")
            .await
    );
    assert!(!rig.service.st.lock().displays.contains_key("display2"));
}

#[tokio::test]
async fn reconciliation_reopens_the_effective_realized_layout() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    let actual = settled_encoder("x264", false);
    *capture.info.lock() = Some(actual);
    *capture.info_delay.lock() = Duration::ZERO;
    rig.service
        .st
        .lock()
        .instances
        .get_mut(PRIMARY_DISPLAY)
        .unwrap()
        .settings
        .video_reference_frames = 0;
    let starts = capture.count("start");
    let layout_guard = rig.service.reconfigure_lock.lock().await;
    let service = rig.service.clone();
    let reconcile =
        tokio::spawn(async move { service.reconcile_reference_frames("primary").await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(capture.count("start"), starts);

    // A realized-root clamp followed classification while reconciliation was reading the encoder.
    rig.service.st.lock().layouts.insert(
        PRIMARY_DISPLAY.to_string(),
        crate::display::Layout::new(0, 0, 1000, 700),
    );
    capture.update_capture_region(0, 0, 1000, 700).unwrap();
    drop(layout_guard);
    reconcile.await.unwrap();

    assert_eq!(capture.count("start"), starts + 1);
    let effective = rig.layout(PRIMARY_DISPLAY).unwrap();
    let last = capture.starts.lock().last().unwrap().clone();
    assert_eq!(
        (last.width, last.height),
        (effective.w, effective.h),
        "a realized-root change cannot be undone by a stale reconciliation reopen"
    );
}

#[tokio::test]
async fn a_failed_start_tells_the_truth_and_the_next_pass_restarts() {
    let rig = Rig::new();
    let mut owner = rig.controller().await;
    let capture = {
        use crate::ws::host::CaptureFactory;
        let _ = rig.captures.capture("spare");
        rig.captures.of("spare").unwrap()
    };
    *capture.fail_start.lock() = true;
    rig.service
        .st
        .lock()
        .persistent
        .insert(PRIMARY_DISPLAY.to_string(), capture.clone());
    owner
        .say(r#"SETTINGS,{"tabId":"a","initialClientWidth":640,"initialClientHeight":480}"#)
        .await;
    settle().await;
    assert!(
        owner.has("VIDEO_STOPPED").await,
        "a capture that did not come up yields a verdict"
    );
    assert!(rig.service.st.lock().displays[PRIMARY_DISPLAY].stop_signaled);
    assert!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].video_active,
        "kept, so the next pass restarts it"
    );
    *capture.fail_start.lock() = false;
    owner.clear().await;
    rig.service.reconfigure_displays().await;
    settle().await;
    assert!(owner.has("VIDEO_STARTED").await);
    assert!(capture.is_capturing_now());
}

#[tokio::test]
async fn the_gate_flushes_the_owner_and_the_lift_asks_for_a_key_frame() {
    let rig = Rig::new();
    let owner = rig.owner("a").await;
    settle().await;
    let capture = rig.primary_capture();
    capture.deliver(1, 1, true);
    settle().await;
    let link = rig.service.link(PRIMARY_DISPLAY).unwrap();
    // Stripes name no reference: the gate gates every row until the lift's key frame.
    {
        let mut l = link.lock();
        l.backpressure_enabled = false;
        l.stall_gated_at = Some(crate::ws::now());
    }
    capture.deliver(2, 2, false);
    settle().await;
    assert_eq!(owner.binaries().len(), 1, "a shut gate skips the owner");
    let idrs = capture.count("idr");
    let at = crate::ws::now();
    {
        let mut l = link.lock();
        l.stall_gated_at = None;
        l.acknowledged_frame_id = -1;
    }
    rig.service
        .backpressure_check(PRIMARY_DISPLAY, &link, at, 0.0)
        .await;
    assert!(
        link.lock().backpressure_enabled,
        "an ack of -1 opens the gate"
    );
    assert_eq!(
        capture.count("idr"),
        idrs + 1,
        "the lift asks for one key frame"
    );
    assert_eq!(
        rig.metrics.keyframe_requests(PRIMARY_DISPLAY, "gate_lift"),
        1,
        "counted as the gate's"
    );
    capture.deliver(3, 3, false);
    settle().await;
    assert_eq!(owner.binaries().len(), 1, "and the deltas ahead of it wait");
    capture.deliver(4, 4, true);
    settle().await;
    assert_eq!(owner.binaries().len(), 2);
}

#[tokio::test]
async fn a_checks_gate_events_reach_the_metrics() {
    let rig = Rig::new();
    let _owner = rig.owner("a").await;
    settle().await;
    rig.primary_capture().deliver(1, 1, true);
    settle().await;
    let link = rig.service.link(PRIMARY_DISPLAY).unwrap();
    let at = crate::ws::now();
    {
        let mut l = link.lock();
        l.acknowledged_frame_id = 1;
        l.has_sent_any_frame = true;
        l.last_sent_frame_id = 1;
        l.unacked_since = Some(at - 60.0);
    }
    rig.service
        .backpressure_check(PRIMARY_DISPLAY, &link, at, 0.0)
        .await;
    rig.service
        .backpressure_check(PRIMARY_DISPLAY, &link, at + 0.1, 0.0)
        .await;
    assert_eq!(link.lock().gate_counts.stalled, 1, "a silent page stalls");
    assert_eq!(
        *rig.metrics.gate_events.lock(),
        vec![(PRIMARY_DISPLAY.to_string(), "stalled", 1)],
        "once, though the gate is checked twice"
    );
}

#[tokio::test]
async fn a_steered_rate_held_is_told_to_its_page() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    owner
        .say(r#"SETTINGS,{"tabId":"a","rate_control_mode":"cbr","video_bitrate":4000}"#)
        .await;
    settle().await;
    owner.clear().await;
    let link = rig.service.link(PRIMARY_DISPLAY).unwrap();
    let capture = rig.primary_capture();
    let steered = crate::settings::settings().congestion_control().on;
    let mut bytes = 0u64;
    for tick in 0..30 {
        let at = crate::ws::now() + f64::from(tick) * 0.5;
        {
            let mut l = link.lock();
            l.acknowledged_frame_id = 1;
            l.has_sent_any_frame = true;
            l.last_sent_frame_id = 1;
            for ack in 0..10 {
                bytes += 25_000;
                l.sent_bytes = bytes;
                l.note_round_trip(20.0, bytes, at - 0.5 + f64::from(ack) * 0.05);
            }
        }
        rig.service
            .backpressure_check(PRIMARY_DISPLAY, &link, at, 0.0)
            .await;
    }
    settle().await;
    if steered {
        assert!(
            owner
                .texts()
                .await
                .iter()
                .any(|t| t.starts_with("CC_RATE ")),
            "{:?}",
            capture.calls.lock()
        );
    }
}

/// A CRF display opens capped at the steer's start, its peak climbs as a CBR target does under
/// the ceiling its largest frame sets (25 kB at 60 frames a second: 12 Mbit/s), its page is
/// told no peak a queue has not shown its path, and a client's bitrate, CBR's alone, leaves
/// the peak where the steer put it.
#[tokio::test]
async fn a_crf_display_is_steered_by_its_peak() {
    use crate::ws::steer::START_KBPS;
    let rig = Rig::new();
    let mut owner = rig.controller().await;
    owner
        .say(r#"SETTINGS,{"initialClientWidth":1280,"initialClientHeight":720,"tabId":"a","encoder":"h264enc","rate_control_mode":"crf"}"#)
        .await;
    settle().await;
    if !crate::settings::settings().congestion_control().on {
        return;
    }
    let capture = rig.primary_capture();
    let opened = capture.starts.lock().last().cloned().expect("a capture");
    assert!(!opened.video_cbr_mode);
    assert_eq!(
        opened.video_vbv_multiplier,
        pixelflux::encoders::CAPPED_VBV_FRAMES,
        "capped from the first frame, so a cut never reopens the encoder"
    );
    assert_eq!(opened.video_bitrate_kbps, START_KBPS as i32);
    owner.clear().await;
    let link = rig.service.link(PRIMARY_DISPLAY).unwrap();
    let hi = crate::settings::settings().video_bitrate().1.as_f64();
    let mut bytes = 0u64;
    for tick in 0..30 {
        let at = crate::ws::now() + f64::from(tick) * 0.5;
        {
            let mut l = link.lock();
            l.acknowledged_frame_id = 1;
            l.has_sent_any_frame = true;
            l.last_sent_frame_id = 1;
            for ack in 0..10 {
                bytes += 25_000;
                l.sent_bytes = bytes;
                l.note_frame(2, 25_000, false, at);
                l.note_round_trip(20.0, bytes, at - 0.5 + f64::from(ack) * 0.05);
            }
        }
        rig.service
            .backpressure_check(PRIMARY_DISPLAY, &link, at, 0.0)
            .await;
    }
    settle().await;
    let peaks: Vec<i64> = capture
        .calls
        .lock()
        .iter()
        .filter_map(|c| c.strip_prefix("bitrate ")?.parse().ok())
        .collect();
    assert!(
        peaks.first().is_some_and(|p| *p > START_KBPS as i64),
        "the peak climbs from the start: {peaks:?}"
    );
    assert!(
        hi > 12_000.0 && peaks.iter().all(|p| *p <= 12_000),
        "under its largest frame's ceiling: {peaks:?}"
    );
    assert!(
        !owner
            .texts()
            .await
            .iter()
            .any(|t| t.starts_with("CC_RATE ")),
        "a peak no queue showed its path is not one to keep: {peaks:?}"
    );
    let calls = capture.calls.lock().len();
    rig.service
        .handle_opcode_video_bitrate(2000.0, PRIMARY_DISPLAY)
        .await;
    assert_eq!(
        capture.calls.lock().len(),
        calls,
        "a client's bitrate moves no peak"
    );
    assert_eq!(
        rig.service.st.lock().displays[PRIMARY_DISPLAY].video_bitrate,
        2000.0,
        "and is kept for CBR"
    );
}

/// A CRF display whose capture says its encoder reads no peak is left to its rate factor, as
/// without congestion control: no steer, no rate told to its page, and its encoder back at the
/// bitrate its client chose.
#[tokio::test]
async fn a_crf_display_whose_encoder_reads_no_peak_is_not_steered() {
    let rig = Rig::new();
    let mut owner = rig.controller().await;
    owner
        .say(r#"SETTINGS,{"initialClientWidth":1280,"initialClientHeight":720,"tabId":"a","encoder":"h264enc","rate_control_mode":"crf","video_bitrate":6000}"#)
        .await;
    settle().await;
    if !crate::settings::settings().congestion_control().on {
        return;
    }
    let watch = rig.service.reg.lock().watches.get(PRIMARY_DISPLAY).cloned();
    let mut described = serde_json::Map::new();
    described.insert("capped".into(), json!(false));
    watch.expect("a watch").describe(described);
    owner.clear().await;
    let capture = rig.primary_capture();
    let calls = capture.calls.lock().len();
    let link = rig.service.link(PRIMARY_DISPLAY).unwrap();
    let mut bytes = 0u64;
    for tick in 0..30 {
        let at = crate::ws::now() + f64::from(tick) * 0.5;
        {
            let mut l = link.lock();
            l.acknowledged_frame_id = 1;
            l.has_sent_any_frame = true;
            l.last_sent_frame_id = 1;
            for ack in 0..10 {
                bytes += 25_000;
                l.sent_bytes = bytes;
                l.note_round_trip(20.0, bytes, at - 0.5 + f64::from(ack) * 0.05);
            }
        }
        rig.service
            .backpressure_check(PRIMARY_DISPLAY, &link, at, 0.0)
            .await;
    }
    settle().await;
    let rates: Vec<String> = capture.calls.lock()[calls..]
        .iter()
        .filter(|c| c.starts_with("bitrate "))
        .cloned()
        .collect();
    assert_eq!(
        rates,
        ["bitrate 6000"],
        "back at the client's bitrate, once"
    );
    assert!(
        !owner
            .texts()
            .await
            .iter()
            .any(|t| t.starts_with("CC_RATE ")),
        "no rate the stream never ran at is remembered"
    );
    // On to an encoder that takes the peak: the new session opens capped at the rate the stream
    // ran at, not at the range's high end.
    assert_eq!(link.lock().link_kbps, None);
    owner
        .say(r#"SETTINGS,{"tabId":"a","encoder":"vp9enc","rate_control_mode":"crf","video_bitrate":6000}"#)
        .await;
    settle().await;
    assert_eq!(
        rig.service.link(PRIMARY_DISPLAY).unwrap().lock().link_kbps,
        Some(6000.0),
        "the encoder's switch left the new capped session at the range's high end"
    );
}

#[tokio::test]
async fn the_clipboard_host_sends_to_one_client_and_waits_on_its_queue() {
    use crate::clipboard::outbound::ClipboardHost;
    let rig = Rig::new();
    let owner = rig.controller().await;
    let viewer = rig.viewer().await;
    let clients = ClipboardHost::clients(rig.service.as_ref());
    assert_eq!(clients.len(), 2);
    assert!(clients.contains(&(viewer.id(), crate::core::Role::Viewer)));
    owner.clear().await;
    viewer.clear().await;
    assert!(
        rig.service
            .send_text(owner.id(), "clipboard,aGk=".into())
            .await
    );
    assert!(owner.has("clipboard,aGk=").await);
    assert!(!viewer.has("clipboard,aGk=").await);
    assert!(
        !rig.service.send_text(9999, "clipboard,aGk=".into()).await,
        "a requester since gone receives nothing"
    );
    let started = crate::ws::now();
    rig.service.bulk_window(owner.id(), 16_000).await;
    assert!(
        crate::ws::now() - started < 1.0,
        "a short queue admits the chunk"
    );
}

#[tokio::test]
async fn capture_demand_goes_to_controllers_on_the_primary() {
    use crate::webcam::demand::DemandTransport;
    let rig = Rig::new();
    let owner = rig.owner("a").await;
    settle().await;
    let _viewer = rig.viewer().await;
    let mut second = rig.controller().await;
    second.say(&secondary_settings("display2", "right")).await;
    settle().await;
    assert_eq!(
        rig.service.capture_candidates(),
        vec![owner.id().to_string()]
    );
    assert!(
        rig.service
            .tell_capture(&owner.id().to_string(), "webcam", true)
            .await
    );
    assert!(owner.has("CAPTURE_DEMAND webcam 1").await);
    assert!(!rig.service.tell_capture("9999", "webcam", true).await);
}

#[tokio::test]
async fn sessions_list_the_pages_and_one_can_be_disconnected() {
    let rig = Rig::new();
    let owner = rig.owner("a").await;
    settle().await;
    let mut perms = owner.client().perms();
    perms.connected_at = 1_000_000.5;
    owner.client().set_perms(perms);
    let sessions = {
        let service = rig.service.clone();
        let task = tokio::spawn(async move { service.sessions().await });
        tokio::time::sleep(Duration::from_millis(5)).await;
        // A page's network process answers the ping.
        let pings: Vec<Bytes> = owner
            .page
            .sent
            .lock()
            .iter()
            .filter_map(|f| match f {
                crate::ws::client::Frame::Ping(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        for ping in pings {
            owner.client().note_pong(&ping);
        }
        task.await.unwrap()
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["transport"], json!("websockets"));
    assert_eq!(sessions[0]["display"], json!("primary"));
    // The form the audit trail and the WebRTC listing carry: UTC to the millisecond.
    assert_eq!(
        sessions[0]["connected_at"],
        json!("1970-01-12T13:46:40.500Z")
    );
    let id = sessions[0]["id"].as_str().unwrap().to_string();
    assert!(rig.service.disconnect_session(&id).await);
    assert_eq!(owner.client().close_code(), Some(1001));
    assert!(!rig.service.disconnect_session("nobody").await);
}

#[tokio::test]
async fn the_ping_for_a_key_frames_arrival_is_written_behind_it() {
    use crate::ws::client::Frame;
    let rig = Rig::new();
    let owner = rig.owner("a").await;
    settle().await;
    let link = rig.service.link(PRIMARY_DISPLAY).unwrap();
    link.lock().expect_key_frame(crate::ws::now());
    owner.clear().await;
    rig.primary_capture().deliver(5, 5, true);
    settle().await;
    let order: Vec<&str> = owner
        .page
        .sent
        .lock()
        .iter()
        .map(|f| match f {
            Frame::Binary(_) => "frame",
            Frame::Ping(_) => "ping",
            _ => "other",
        })
        .collect();
    let frame = order
        .iter()
        .position(|f| *f == "frame")
        .expect("the key frame");
    assert!(
        order[frame + 1..].contains(&"ping") && !order[..frame].contains(&"ping"),
        "a ping ahead of the frame would report it arrived before it left: {order:?}"
    );
}

// --- A single-display Wayland resize -------------------------------------------------------------

/// What the screens were asked since the last call, the capture compositor's and the session's.
fn sized(rig: &Rig) -> Vec<String> {
    std::mem::take(&mut *rig.display.calls.lock())
        .into_iter()
        .filter(|c| c.starts_with("screen") || c.starts_with("realize"))
        .collect()
}

/// A nested session's screen is its own compositor's, so the in-place resize carries it along;
/// the Wayland setting is read once per process.
#[tokio::test]
#[ignore = "run by a_lone_wayland_resize_carries_the_session_screen, in a process of its own"]
async fn a_lone_wayland_resize_in_its_process() {
    if !crate::session_screens::testing::isolated() {
        return;
    }
    crate::settings::install(crate::settings::Settings::parse(
        &["--wayland=true".to_string()],
        &|_| None,
    ));
    let rig = Rig::new();
    let mut page = rig.controller().await;
    page.say(r#"SETTINGS,{"initialClientWidth":1280,"initialClientHeight":720,"tabId":"a"}"#)
        .await;
    let capture = rig.primary_capture();
    {
        let mut st = rig.service.st.lock();
        let d = st.displays.get_mut(PRIMARY_DISPLAY).expect("the primary");
        d.scaling_dpi = Some("192".into());
    }

    // The compositor even-masks to 1920x928; the session must land there too.
    *capture.geometry.lock() = Ok(Some((1920, 928, 1.0)));
    sized(&rig);
    rig.service.handle_resize("1920x936", PRIMARY_DISPLAY).await;
    assert_eq!(
        sized(&rig),
        [
            "screen 1920x936 grow_only=true",
            "screen 1920x928 grow_only=false",
            "realize 192 primary Some((1920, 928))",
        ],
        "the capture screen is grown, then fitted to what was realized, and the session's sized \
         once, to the realized geometry, at the DPI the display holds, for the display being resized"
    );

    // At DPR 2 the media resizes, at DPR 1 it comes back, and the session screen arrives at both.
    *capture.geometry.lock() = Ok(None);
    rig.service
        .handle_resize("2400x1440", PRIMARY_DISPLAY)
        .await;
    assert_eq!(
        sized(&rig).last().map(String::as_str),
        Some("realize 192 primary Some((2400, 1440))")
    );
    rig.service
        .st
        .lock()
        .displays
        .get_mut(PRIMARY_DISPLAY)
        .expect("the primary")
        .scaling_dpi = Some("96".into());
    rig.service.handle_resize("1920x936", PRIMARY_DISPLAY).await;
    assert_eq!(
        sized(&rig).last().map(String::as_str),
        Some("realize 96 primary Some((1920, 936))"),
        "a resize after a DPI change moves the session screen with it"
    );

    rig.service.handle_resize("1920x936", PRIMARY_DISPLAY).await;
    assert!(sized(&rig).is_empty(), "a redundant resize sizes nothing");

    rig.service.handle_resize("800x600", "display9").await;
    assert!(
        sized(&rig).is_empty(),
        "a display nobody holds sizes nothing"
    );
}

/// A pass that laid the display out at its old size is in flight when the resize arrives.
#[tokio::test]
#[ignore = "run by a_lone_wayland_resize_during_a_pass_is_the_size_kept, in a process of its own"]
async fn a_lone_wayland_resize_during_a_pass_in_its_process() {
    if !crate::session_screens::testing::isolated() {
        return;
    }
    crate::settings::install(crate::settings::Settings::parse(
        &["--wayland=true".to_string()],
        &|_| None,
    ));
    let rig = Rig::new();
    let mut page = rig.controller().await;
    page.say(r#"SETTINGS,{"initialClientWidth":1280,"initialClientHeight":720,"tabId":"a"}"#)
        .await;
    let hold = Arc::new(tokio::sync::Notify::new());
    *rig.display.hold_layout.lock() = Some(hold.clone());
    let service = rig.service.clone();
    let pass = tokio::spawn(async move { service.reconfigure_displays().await });
    while rig.display.hold_layout.lock().is_some() {
        settle().await;
    }
    sized(&rig);
    rig.service
        .handle_resize("1920x1080", PRIMARY_DISPLAY)
        .await;
    hold.notify_one();
    pass.await.unwrap();
    assert_eq!(
        rig.layout(PRIMARY_DISPLAY).map(|l| (l.w, l.h)),
        Some((1920, 1080)),
        "the pass cannot lay the display out at the size it read before the resize"
    );
    let screens: Vec<String> = sized(&rig)
        .into_iter()
        .filter(|c| c.starts_with("screen"))
        .collect();
    assert_eq!(
        screens.last().map(String::as_str),
        Some("screen 1920x1080 grow_only=false"),
        "and the screen ends at the resize: {screens:?}"
    );
}

#[test]
fn a_lone_wayland_resize_during_a_pass_is_the_size_kept() {
    crate::session_screens::testing::run_isolated(
        "ws::server_tests::a_lone_wayland_resize_during_a_pass_in_its_process",
        &[],
    );
}

#[test]
fn a_lone_wayland_resize_carries_the_session_screen() {
    crate::session_screens::testing::run_isolated(
        "ws::server_tests::a_lone_wayland_resize_in_its_process",
        &[],
    );
}

/// A realized-geometry read folds in what the compositor made of the size a pass laid out, and
/// leaves a size the page asked for during the pass to the pass that lays it out.
#[tokio::test]
#[ignore = "run by a_request_during_a_pass_outlives_its_realized_read, in a process of its own"]
async fn a_request_during_a_pass_in_its_process() {
    if !crate::session_screens::testing::isolated() {
        return;
    }
    crate::settings::install(crate::settings::Settings::parse(
        &["--wayland=true".to_string()],
        &|_| None,
    ));
    let rig = Rig::new();
    let mut page = rig.controller().await;
    page.say(r#"SETTINGS,{"initialClientWidth":1280,"initialClientHeight":720,"tabId":"a"}"#)
        .await;
    let capture = rig.primary_capture();
    let size = |rig: &Rig| {
        let st = rig.service.st.lock();
        let d = &st.displays[PRIMARY_DISPLAY];
        (d.width, d.height)
    };

    // The pass laid 1920x1080 out, and the page asked for 1600x838 while it ran.
    {
        let mut st = rig.service.st.lock();
        let d = st.displays.get_mut(PRIMARY_DISPLAY).expect("the primary");
        d.laid_out = Some((1920, 1080));
        (d.width, d.height) = (1600, 838);
    }
    *capture.geometry.lock() = Ok(Some((1920, 1080, 1.0)));
    rig.service
        .sync_wayland_realized_geometry(PRIMARY_DISPLAY, false)
        .await;
    assert_eq!(size(&rig), (1600, 838), "the request outlives the read");
    let layout = rig.layout(PRIMARY_DISPLAY).expect("the primary's layout");
    assert_eq!(
        (layout.w, layout.h),
        (1920, 1080),
        "the layout holds what was realized"
    );

    // Laid out in turn, the request takes what the compositor made of it.
    rig.service
        .st
        .lock()
        .displays
        .get_mut(PRIMARY_DISPLAY)
        .expect("the primary")
        .laid_out = Some((1600, 838));
    *capture.geometry.lock() = Ok(Some((1600, 832, 1.0)));
    rig.service
        .sync_wayland_realized_geometry(PRIMARY_DISPLAY, false)
        .await;
    assert_eq!(size(&rig), (1600, 832));
}

#[test]
fn a_request_during_a_pass_outlives_its_realized_read() {
    crate::session_screens::testing::run_isolated(
        "ws::server_tests::a_request_during_a_pass_in_its_process",
        &[],
    );
}

/// The desktop follows the primary display's page alone, through the shared sanitizer: a
/// secondary's page and a viewer's ask for nothing, and a value outside the list asks for the
/// server's own.
#[tokio::test]
async fn the_color_scheme_is_the_primary_pages_alone() {
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    owner
        .say(r#"SETTINGS,{"tabId":"a","color_scheme":"dark"}"#)
        .await;
    settle().await;
    let mut second = rig.controller().await;
    second
        .say(r#"SETTINGS,{"displayId":"display2","displayPosition":"left","initialClientWidth":800,"initialClientHeight":600,"color_scheme":"light"}"#)
        .await;
    let mut viewer = rig.viewer().await;
    viewer
        .say(r#"SETTINGS,{"tabId":"v","color_scheme":"light"}"#)
        .await;
    owner
        .say(r#"SETTINGS,{"tabId":"a","color_scheme":"sepia"}"#)
        .await;
    settle().await;
    assert_eq!(
        *rig.input.color_schemes.lock(),
        ["dark", "off"],
        "the primary's page asks, and a scheme outside the list is the server's own"
    );
}

#[tokio::test]
#[ignore = "run by an_operators_color_scheme_is_locked, in a process of its own"]
async fn an_operators_color_scheme_is_locked_in_its_process() {
    if !crate::session_screens::testing::isolated() {
        return;
    }
    crate::settings::install(crate::settings::Settings::parse(
        &["--color-scheme=dark".to_string()],
        &|_| None,
    ));
    let rig = Rig::new();
    let mut owner = rig.owner("a").await;
    settle().await;
    owner
        .say(r#"SETTINGS,{"tabId":"a","color_scheme":"light"}"#)
        .await;
    settle().await;
    assert_eq!(
        *rig.input.color_schemes.lock(),
        ["dark"],
        "a page's scheme gives way to the one the operator locked"
    );
}

#[test]
fn an_operators_color_scheme_is_locked() {
    crate::session_screens::testing::run_isolated(
        "ws::server_tests::an_operators_color_scheme_is_locked_in_its_process",
        &[],
    );
}
