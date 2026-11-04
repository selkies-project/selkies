/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a hello may name and take over, what a close tells a page, a session outliving a cut
//! of its socket, and who owns a display.
//!
//! The registry runs with scripted sockets, through `serve`, the path the WebSocket route
//! takes; no server, no browser. A `Sock` records what the registry wrote to it, and on a
//! close frame ends its inbound side the way a WebSocket does.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::lifecycle::{DisplayGraphs, Release, StopGrace};
use super::registry::{
    HELD_LINES_MAX, PeerRegistry, SessionAuthority, SignalingOptions, TokenPermissions,
};
use super::socket::{Inbound, Outbound, PeerSocket, socket};
use super::wire::{ClientType, HELLO_MAX_BYTES, SessionStart, hello_page_size};
use super::{ClientEvent, RegistryConnector, SignalingClient};

const KEY: &str = "0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct Written {
    sent: Vec<String>,
    close: Option<(u16, String)>,
}

/// A signaling socket fed by the test.
struct Sock {
    feed: mpsc::Sender<Inbound>,
    socket: Arc<PeerSocket>,
    written: Arc<Mutex<Written>>,
    task: JoinHandle<()>,
}

impl Sock {
    /// Connect a socket to `registry` and feed it `lines`.
    async fn join(
        registry: &Arc<PeerRegistry>,
        ceiling: Option<ClientType>,
        lines: &[&str],
    ) -> Sock {
        let (peer_socket, mut outbox) = socket();
        let (feed, inbound) = mpsc::channel(4096);
        let written = Arc::new(Mutex::new(Written::default()));
        let sink = Arc::clone(&written);
        let closing = feed.clone();
        tokio::spawn(async move {
            while let Some(outbound) = outbox.recv().await {
                match outbound {
                    Outbound::Text(line) => sink.lock().unwrap().sent.push(line),
                    Outbound::Close { code, reason } => {
                        sink.lock().unwrap().close = Some((code, reason));
                        let _ = closing.send(Inbound::CloseFrame).await;
                    }
                    Outbound::Abort | Outbound::Ping(_) | Outbound::Pong(_) => {}
                }
            }
        });
        let served = Arc::clone(registry);
        let serve_socket = Arc::clone(&peer_socket);
        let task = tokio::spawn(async move {
            served.serve(serve_socket, inbound, "page", ceiling).await;
        });
        let sock = Sock {
            feed,
            socket: peer_socket,
            written,
            task,
        };
        for line in lines {
            sock.send(line).await;
        }
        settle().await;
        sock
    }

    async fn send(&self, line: &str) {
        let _ = self.feed.send(Inbound::Text(line.to_owned())).await;
    }

    async fn cut(&self) {
        let _ = self.feed.send(Inbound::Cut).await;
        settle().await;
    }

    async fn close_frame(&self) {
        let _ = self.feed.send(Inbound::CloseFrame).await;
        settle().await;
    }

    fn sent(&self) -> Vec<String> {
        self.written.lock().unwrap().sent.clone()
    }

    fn close_args(&self) -> Option<(u16, String)> {
        self.written.lock().unwrap().close.clone()
    }

    fn close_code(&self) -> Option<u16> {
        self.close_args().map(|(code, _)| code)
    }

    fn closed(&self) -> bool {
        self.close_args().is_some()
    }

    fn started(&self) -> Vec<String> {
        self.sent()
            .iter()
            .filter(|line| line.starts_with("SESSION_START"))
            .map(|line| line.split(' ').nth(1).unwrap_or("").to_owned())
            .collect()
    }

    fn ended(&self) -> Vec<String> {
        self.sent()
            .iter()
            .filter(|line| line.starts_with("SESSION_END"))
            .map(|line| line.split(' ').nth(1).unwrap_or("").to_owned())
            .collect()
    }
}

/// Until the registry's tasks ran what the test set off.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(15)).await;
}

fn options(sharing: bool) -> SignalingOptions {
    SignalingOptions {
        enable_sharing: sharing,
        ..SignalingOptions::default()
    }
}

fn hello_json(meta: Value) -> String {
    format!("HELLO client {meta}")
}

fn hello(client_type: &str, slot: i64) -> String {
    hello_json(json!({"client_type": client_type, "client_slot": slot}))
}

fn resumable(client_type: &str, key: &str) -> String {
    let mut meta = json!({"client_type": client_type, "client_slot": -1});
    if !key.is_empty() {
        meta["resume_key"] = json!(key);
    }
    hello_json(meta)
}

// A hello's bounds

#[tokio::test]
async fn a_view_only_page_leaves_the_live_controller_and_a_newer_controller_takes_over() {
    let registry = PeerRegistry::new(options(false));
    let controller = Sock::join(&registry, None, &[&hello("controller", 1)]).await;
    for claim in ["controller", "viewer"] {
        let viewer = Sock::join(&registry, Some(ClientType::Viewer), &[&hello(claim, 1)]).await;
        assert!(
            !controller.closed() && !viewer.closed(),
            "a view-only page claiming {claim}: {:?} {:?}",
            controller.close_args(),
            viewer.close_args()
        );
    }
    let newer = Sock::join(&registry, None, &[&hello("controller", 1)]).await;
    assert!(
        controller.closed(),
        "a newer controller page takes the display over"
    );
    assert!(!newer.closed());
}

#[tokio::test]
async fn a_hello_past_the_bound_is_refused_unread() {
    let registry = PeerRegistry::new(options(true));
    let big = hello_json(
        json!({"client_type": "controller", "client_slot": 1, "pad": "x".repeat(HELLO_MAX_BYTES)}),
    );
    let sock = Sock::join(&registry, None, &[&big]).await;
    assert_eq!(sock.close_code(), Some(1009));
    assert!(registry.peer_ids().is_empty());
}

#[tokio::test]
async fn a_display_id_is_a_short_name_or_refused() {
    let registry = PeerRegistry::new(options(true));
    let long = "x".repeat(33);
    for bad in [
        json!("primary 0 right forged-token"),
        json!(long),
        json!(""),
        json!(7),
        json!(["primary"]),
        json!({"a": 1}),
    ] {
        let line =
            hello_json(json!({"client_type": "controller", "client_slot": 1, "display_id": bad}));
        let sock = Sock::join(&registry, None, &[&line]).await;
        if bad == json!("") {
            assert!(!sock.closed(), "an empty display_id is taken as primary");
        } else {
            assert_eq!(sock.close_code(), Some(1002), "{bad}");
        }
        registry.clear_peers();
    }
    let line = hello_json(
        json!({"client_type": "controller", "client_slot": 1, "display_id": "display2"}),
    );
    assert!(!Sock::join(&registry, None, &[&line]).await.closed());
}

#[tokio::test]
async fn the_storm_table_forgets_identities_whose_takeovers_left_the_window() {
    let registry = PeerRegistry::new(SignalingOptions {
        eviction_storm_window: Duration::from_millis(50),
        ..options(false)
    });
    for i in 0..20 {
        let line = hello_json(
            json!({"client_type": "controller", "client_slot": 1, "display_id": format!("d{i}")}),
        );
        Sock::join(&registry, None, &[&line]).await;
        Sock::join(&registry, None, &[&line]).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let line =
        hello_json(json!({"client_type": "controller", "client_slot": 1, "display_id": "last"}));
    Sock::join(&registry, None, &[&line]).await;
    Sock::join(&registry, None, &[&line]).await;
    assert_eq!(registry.storm_identities(), 1);
}

// What a close tells a page

#[tokio::test]
async fn the_server_peer_leaving_closes_every_page_as_going_away() {
    let registry = PeerRegistry::new(options(true));
    let server_hello = format!(
        "HELLO server {}",
        json!({"server_token": registry.server_peer_secret()})
    );
    let server = Sock::join(&registry, None, &[&server_hello]).await;
    let a = Sock::join(&registry, None, &[&hello("controller", -1)]).await;
    let b = Sock::join(&registry, None, &[&hello("viewer", -1)]).await;
    server.close_frame().await;
    settle().await;
    assert_eq!(a.close_code(), Some(1001));
    assert_eq!(b.close_code(), Some(1001));
}

#[tokio::test]
async fn a_page_still_registered_when_the_service_stops_is_closed_as_going_away() {
    let registry = PeerRegistry::new(options(true));
    let late = Sock::join(&registry, None, &[&hello("controller", -1)]).await;
    registry.close_clients();
    settle().await;
    assert_eq!(late.close_code(), Some(1001));
}

#[tokio::test]
async fn a_slot_the_server_does_not_offer_is_refused_with_the_verdict_and_its_reason() {
    let registry = PeerRegistry::new(options(true));
    let refused = Sock::join(&registry, None, &[&hello("viewer", 3)]).await;
    let (code, reason) = refused.close_args().expect("closed");
    assert_eq!(code, 4000);
    assert!(reason.contains("player"), "{reason}");
}

#[tokio::test]
async fn a_page_a_newer_controller_superseded_is_told_so_not_asked_back() {
    let registry = PeerRegistry::new(options(true));
    let first = Sock::join(&registry, None, &[&hello("controller", -1)]).await;
    let second = Sock::join(&registry, None, &[&hello("controller", -1)]).await;
    assert_eq!(first.close_code(), Some(4001));
    assert!(!second.closed());
}

#[tokio::test]
async fn a_server_peer_proves_its_role() {
    let registry = PeerRegistry::new(options(true));
    let no_proof = Sock::join(&registry, None, &["HELLO server"]).await;
    assert_eq!(no_proof.close_code(), Some(4001));
    let wrong = Sock::join(
        &registry,
        None,
        &[r#"HELLO server {"server_token": "guess"}"#],
    )
    .await;
    assert_eq!(wrong.close_code(), Some(4001));
}

#[tokio::test]
async fn hellos_that_are_no_hellos_are_refused_as_protocol_errors() {
    let registry = PeerRegistry::new(options(true));
    for line in [
        "HELLO",
        "NOPE client {}",
        "HELLO alien",
        "HELLO client not-json",
        "HELLO client [1]",
        "HELLO client {\"client_type\": \"admin\", \"client_slot\": 1}",
        "HELLO client {\"client_type\": \"viewer\", \"client_slot\": true}",
    ] {
        let sock = Sock::join(&registry, None, &[line]).await;
        assert_eq!(sock.close_code(), Some(1002), "{line}");
    }
    assert!(registry.peer_ids().is_empty());
}

// What a hello carries

#[test]
fn capabilities_sit_beside_the_token_in_session_start() {
    let seen =
        SessionStart::parse("SESSION_START p1 controller primary right tok fullcolor=h264,vp9")
            .unwrap();
    assert_eq!(seen.client_token.as_deref(), Some("tok"));
    assert_eq!(
        seen.fullcolor_codecs,
        Some(vec!["h264".to_owned(), "vp9".to_owned()])
    );
    assert_eq!(
        (seen.display_id.as_str(), seen.display_position.as_str()),
        ("primary", "right")
    );

    let seen = SessionStart::parse("SESSION_START p1 controller second left fullcolor=").unwrap();
    assert_eq!(seen.client_token, None);
    assert_eq!(seen.fullcolor_codecs, Some(vec![]));

    let seen = SessionStart::parse(
        "SESSION_START p1 controller primary right tok fullcolor=h264 tenbit=av1,vp9,vp9444",
    )
    .unwrap();
    assert_eq!(seen.client_token.as_deref(), Some("tok"));
    assert_eq!(seen.fullcolor_codecs, Some(vec!["h264".to_owned()]));
    assert_eq!(
        seen.tenbit_codecs,
        Some(vec![
            "av1".to_owned(),
            "vp9".to_owned(),
            "vp9444".to_owned()
        ])
    );
    let seen = SessionStart::parse("SESSION_START p1 controller primary right tenbit=").unwrap();
    assert_eq!(seen.client_token, None);
    assert_eq!(seen.fullcolor_codecs, None);
    assert_eq!(seen.tenbit_codecs, Some(vec![]));
    assert_eq!(
        SessionStart::parse("SESSION_START p1 viewer primary right tok")
            .unwrap()
            .tenbit_codecs,
        None
    );

    assert_eq!(
        SessionStart::parse("SESSION_START p1 viewer primary right")
            .unwrap()
            .fullcolor_codecs,
        None
    );
    let legacy = SessionStart::parse("SESSION_START p1 viewer").unwrap();
    assert_eq!(legacy.fullcolor_codecs, None);
    assert_eq!(legacy.display_id, "primary");
    assert_eq!(SessionStart::parse("SESSION_START p1 viewer primary"), None);
}

#[test]
fn a_hellos_page_size_is_two_whole_numbers_or_nothing() {
    assert_eq!(
        hello_page_size(Some(&json!([1280, 720]))),
        Some((1280, 720))
    );
    for bad in [
        json!(null),
        json!([1280]),
        json!([1280, 720, 1]),
        json!([0, 720]),
        json!([1280, 9000]),
        json!([1280.0, 720]),
        json!(["1280", "720"]),
        json!([true, 720]),
        json!("1280x720"),
    ] {
        assert_eq!(hello_page_size(Some(&bad)), None, "{bad}");
    }
}

#[tokio::test]
async fn what_a_hello_said_reaches_session_start_and_the_peer_entry() {
    let registry = PeerRegistry::new(options(true));
    let server_hello = format!(
        "HELLO server {}",
        json!({"server_token": registry.server_peer_secret()})
    );
    let server = Sock::join(&registry, None, &[&server_hello]).await;
    let page = hello_json(json!({
        "client_type": "controller", "client_slot": 1.0, "client_token": "tok",
        "display_position": "left", "fullcolor_codecs": ["h264", "bad,name", 7],
        "tenbit_codecs": [], "page_size": [1280, 720], "client_tab_id": "t".repeat(80),
        "cc_start_kbps": 4000, "display_id": "display2",
    }));
    let page = Sock::join(&registry, None, &[&page, "SESSION server"]).await;
    let start = server
        .sent()
        .into_iter()
        .find(|l| l.starts_with("SESSION_START"))
        .expect("a start");
    let uid = start.split(' ').nth(1).unwrap().to_owned();
    assert_eq!(
        start,
        format!("SESSION_START {uid} controller display2 left tok fullcolor=h264 tenbit=")
    );
    assert!(
        page.sent()[1].starts_with("SESSION_OK server-"),
        "{:?}",
        page.sent()
    );
    let info = registry.peer(&uid).expect("registered");
    assert_eq!(info.client_slot, Some(1));
    assert_eq!(info.page_size, Some((1280, 720)));
    assert_eq!(info.tab_id.as_deref().map(str::len), Some(64));
    assert_eq!(info.cc_start_kbps, Some(json!(4000)));
}

#[tokio::test]
async fn a_token_with_whitespace_cannot_forge_session_start_fields() {
    let registry = PeerRegistry::new(options(true));
    let server_hello = format!(
        "HELLO server {}",
        json!({"server_token": registry.server_peer_secret()})
    );
    let server = Sock::join(&registry, None, &[&server_hello]).await;
    let page = hello_json(
        json!({"client_type": "viewer", "client_slot": -1, "client_token": "x fullcolor=h264"}),
    );
    Sock::join(&registry, None, &[&page, "SESSION server"]).await;
    let start = server
        .sent()
        .into_iter()
        .find(|l| l.starts_with("SESSION_START"))
        .expect("a start");
    let seen = SessionStart::parse(&start).expect("parses");
    assert_eq!(seen.client_token, None);
    assert_eq!(seen.fullcolor_codecs, None);
}

// A session outliving a cut of its socket

struct Rig {
    registry: Arc<PeerRegistry>,
    server: Sock,
    page: Sock,
    uid: String,
    hold: Duration,
}

impl Rig {
    async fn start(sharing: bool, client_type: &str, key: &str) -> Rig {
        let hold = Duration::from_millis(200);
        let registry = PeerRegistry::new(SignalingOptions {
            session_hold: hold,
            ..options(sharing)
        });
        let server_hello = format!(
            "HELLO server {}",
            json!({"server_token": registry.server_peer_secret()})
        );
        let server = Sock::join(&registry, None, &[&server_hello]).await;
        let page = Sock::join(
            &registry,
            None,
            &[&resumable(client_type, key), "SESSION server"],
        )
        .await;
        let uid = server.started().last().cloned().expect("a session started");
        Rig {
            registry,
            server,
            page,
            uid,
            hold,
        }
    }

    async fn join(&self, lines: &[&str]) -> Sock {
        Sock::join(&self.registry, None, lines).await
    }
}

#[tokio::test]
async fn a_cut_pages_session_is_held_and_a_hello_naming_its_key_resumes_it() {
    let rig = Rig::start(true, "controller", KEY).await;
    rig.page.cut().await;
    rig.server
        .send(&format!(r#"{} {{"ice": 1}}"#, rig.uid))
        .await;
    rig.server
        .send(&format!(r#"{} {{"ice": 2}}"#, rig.uid))
        .await;
    settle().await;
    assert!(
        rig.server.ended().is_empty(),
        "the server peer hears nothing"
    );
    assert!(rig.registry.session_partner(&rig.uid).is_some());
    assert_eq!(rig.registry.held_lines(&rig.uid).len(), 2);

    let back = rig.join(&[&resumable("controller", KEY)]).await;
    let server_id = rig
        .registry
        .session_partner(&rig.uid)
        .expect("still in session");
    assert_eq!(
        back.sent(),
        vec![
            format!("RESUMED {server_id}"),
            format!(r#"{server_id} {{"ice": 1}}"#),
            format!(r#"{server_id} {{"ice": 2}}"#),
        ]
    );
    assert_eq!(rig.registry.socket_of(&rig.uid), Some(back.socket.id()));
    assert!(rig.registry.held_lines(&rig.uid).is_empty());

    tokio::time::sleep(rig.hold * 2).await;
    rig.server
        .send(&format!(r#"{} {{"ice": 3}}"#, rig.uid))
        .await;
    back.send(&format!(r#"{server_id} {{"ice": 4}}"#)).await;
    settle().await;
    assert!(
        rig.server.ended().is_empty(),
        "the resumed session outlives the hold"
    );
    assert_eq!(
        rig.server.started(),
        vec![rig.uid.clone()],
        "no second start"
    );
    assert_eq!(
        back.sent().last().unwrap(),
        &format!(r#"{server_id} {{"ice": 3}}"#)
    );
    assert_eq!(
        rig.server.sent().last().unwrap(),
        &format!(r#"{} {{"ice": 4}}"#, rig.uid)
    );
}

#[tokio::test]
async fn a_session_nobody_resumes_ends_after_the_hold_and_a_later_hello_is_a_new_page() {
    let rig = Rig::start(true, "controller", KEY).await;
    rig.page.cut().await;
    tokio::time::sleep(rig.hold * 2).await;
    assert_eq!(rig.server.ended(), vec![rig.uid.clone()]);
    assert!(rig.registry.peer(&rig.uid).is_none());
    let late = rig.join(&[&resumable("controller", KEY)]).await;
    assert_eq!(late.sent().first().map(String::as_str), Some("HELLO"));
}

#[tokio::test]
async fn a_close_frame_ends_the_session_at_once() {
    let rig = Rig::start(true, "controller", KEY).await;
    rig.page.close_frame().await;
    assert_eq!(rig.server.ended(), vec![rig.uid.clone()]);
    assert!(rig.registry.peer(&rig.uid).is_none());
}

#[tokio::test]
async fn another_key_gets_a_session_of_its_own_and_the_held_one_ends() {
    let rig = Rig::start(false, "controller", KEY).await;
    rig.page.cut().await;
    let other = rig
        .join(&[&resumable("controller", &"f".repeat(32)), "SESSION server"])
        .await;
    assert_eq!(other.sent().first().map(String::as_str), Some("HELLO"));
    assert_eq!(rig.server.ended(), vec![rig.uid.clone()]);
    assert_eq!(rig.server.started().len(), 2);
}

#[tokio::test]
async fn the_same_key_in_another_role_resumes_nothing() {
    let rig = Rig::start(true, "controller", KEY).await;
    rig.page.cut().await;
    let viewer = rig.join(&[&resumable("viewer", KEY)]).await;
    assert_eq!(viewer.sent().first().map(String::as_str), Some("HELLO"));
    assert_eq!(rig.registry.socket_of(&rig.uid), Some(rig.page.socket.id()));
}

#[tokio::test]
async fn a_page_that_named_no_key_ends_at_once_on_a_cut() {
    let rig = Rig::start(true, "controller", "").await;
    rig.page.cut().await;
    assert_eq!(rig.server.ended(), vec![rig.uid.clone()]);
}

#[tokio::test]
async fn a_cut_the_server_did_not_see_resumes_and_the_stale_socket_closing_ends_nothing() {
    let rig = Rig::start(true, "controller", KEY).await;
    let fresh = rig.join(&[&resumable("controller", KEY)]).await;
    let server_id = rig.registry.session_partner(&rig.uid).expect("in session");
    assert_eq!(fresh.sent().first(), Some(&format!("RESUMED {server_id}")));
    assert_eq!(rig.page.close_code(), Some(1000));
    tokio::time::sleep(rig.hold * 2).await;
    assert!(rig.server.ended().is_empty());
    assert_eq!(rig.registry.socket_of(&rig.uid), Some(fresh.socket.id()));
}

#[tokio::test]
async fn relaying_to_a_cut_page_carries_on_and_what_it_holds_is_bounded() {
    let rig = Rig::start(true, "controller", KEY).await;
    rig.page.cut().await;
    for i in 0..HELD_LINES_MAX + 10 {
        rig.server
            .send(&format!(r#"{} {{"ice": {i}}}"#, rig.uid))
            .await;
    }
    settle().await;
    assert!(!rig.server.closed());
    assert_eq!(rig.registry.held_lines(&rig.uid).len(), HELD_LINES_MAX);
}

#[tokio::test]
async fn a_relay_goes_only_between_session_partners() {
    let rig = Rig::start(true, "controller", KEY).await;
    let other = rig
        .join(&[&resumable("viewer", &"e".repeat(32)), "SESSION server"])
        .await;
    let other_uid = rig.server.started().last().cloned().unwrap();
    let server_id = rig.registry.session_partner(&rig.uid).unwrap();
    rig.page
        .send(&format!(r#"{other_uid} {{"ice": "forged"}}"#))
        .await;
    settle().await;
    assert!(!other.sent().iter().any(|l| l.contains("forged")));
    rig.page
        .send(&format!(r#"{server_id} {{"ice": "real"}}"#))
        .await;
    settle().await;
    assert!(
        rig.server
            .sent()
            .iter()
            .any(|l| l == &format!(r#"{} {{"ice": "real"}}"#, rig.uid))
    );
}

#[tokio::test]
async fn a_session_is_only_with_the_streaming_server() {
    let registry = PeerRegistry::new(options(true));
    let a = Sock::join(
        &registry,
        None,
        &[&hello("controller", -1), "SESSION server"],
    )
    .await;
    assert_eq!(
        a.sent().last().map(String::as_str),
        Some("ERROR peer server not found")
    );
}

#[tokio::test]
async fn rooms_relay_between_members() {
    let registry = PeerRegistry::new(options(true));
    let a = Sock::join(&registry, None, &[&hello("viewer", -1), "ROOM r1"]).await;
    let a_uid = registry.peer_ids().pop().unwrap();
    let b = Sock::join(&registry, None, &[&hello("viewer", -1), "ROOM r1"]).await;
    assert_eq!(b.sent()[1], format!("ROOM_OK {a_uid}"));
    let b_uid = a
        .sent()
        .last()
        .unwrap()
        .strip_prefix("ROOM_PEER_JOINED ")
        .unwrap()
        .to_owned();
    b.send(&format!("ROOM_PEER_MSG {a_uid} hi there")).await;
    settle().await;
    assert_eq!(
        a.sent().last().unwrap(),
        &format!("ROOM_PEER_MSG {b_uid} hi there")
    );
    b.send("nonsense").await;
    settle().await;
    assert_eq!(
        b.sent().last().map(String::as_str),
        Some("ERROR invalid msg, already in room")
    );
    let c = Sock::join(&registry, None, &[&hello("viewer", -1), "ROOM session"]).await;
    assert!(
        c.sent()
            .last()
            .unwrap()
            .starts_with("ERROR invalid room id")
    );
    b.close_frame().await;
    assert_eq!(a.sent().last().unwrap(), &format!("ROOM_PEER_LEFT {b_uid}"));
}

// Who a page is

struct Tokens(HashMap<&'static str, ClientType>);

impl SessionAuthority for Tokens {
    fn master_token(&self) -> Option<String> {
        Some("unit-master".to_owned())
    }

    fn lookup(&self, token: &str) -> Option<TokenPermissions> {
        self.0
            .get(token)
            .map(|role| TokenPermissions { role: *role })
    }
}

fn page(client_type: &str, token: Option<&str>, tab: &str, slot: i64) -> String {
    let mut meta = json!({"client_type": client_type, "client_slot": slot, "client_tab_id": tab});
    if let Some(token) = token {
        meta["client_token"] = json!(token);
    }
    hello_json(meta)
}

#[tokio::test]
async fn with_tokens_a_page_supersedes_only_a_page_of_its_own_token() {
    let registry = PeerRegistry::new(SignalingOptions {
        authority: Arc::new(Tokens(HashMap::from([
            ("ctl-Rk2", ClientType::Controller),
            ("ann-Hq7", ClientType::Viewer),
            ("bo-Zt9", ClientType::Viewer),
        ]))),
        ..options(true)
    });
    let controller = Sock::join(
        &registry,
        None,
        &[&page("controller", Some("ctl-Rk2"), "tab-c", 1)],
    )
    .await;
    let ann = Sock::join(
        &registry,
        None,
        &[&page("controller", Some("ann-Hq7"), "tab-a", 1)],
    )
    .await;
    let bo = Sock::join(
        &registry,
        None,
        &[&page("controller", Some("bo-Zt9"), "tab-b", 1)],
    )
    .await;
    assert!(!controller.closed() && !ann.closed() && !bo.closed());
    assert_eq!(
        registry.peer_ids().len(),
        3,
        "the collaboration room all stays"
    );

    let again = Sock::join(
        &registry,
        None,
        &[&page("controller", Some("ann-Hq7"), "tab-a", 1)],
    )
    .await;
    assert_eq!(ann.close_code(), Some(4001));
    assert!(!again.closed() && !controller.closed() && !bo.closed());
    assert_eq!(registry.peer_ids().len(), 3);

    let mut churn = Vec::new();
    for i in 0..6 {
        churn.push(
            Sock::join(
                &registry,
                None,
                &[&page("controller", Some("bo-Zt9"), &format!("tab-{i}"), 1)],
            )
            .await,
        );
    }
    assert!(
        churn.iter().any(|sock| sock.close_code() == Some(4000)),
        "the storm breaker trips"
    );
    assert!(!controller.closed() && !again.closed());

    let bad = Sock::join(
        &registry,
        None,
        &[&page("viewer", Some("no-such"), "tab-x", 1)],
    )
    .await;
    assert_eq!(bad.close_code(), Some(4001));
    let none = Sock::join(&registry, None, &[&page("viewer", None, "tab-y", 1)]).await;
    assert_eq!(none.close_code(), Some(4001));
}

#[tokio::test]
async fn a_viewer_tokens_controller_claim_is_coerced_to_viewer() {
    let registry = PeerRegistry::new(SignalingOptions {
        authority: Arc::new(Tokens(HashMap::from([("ann-Hq7", ClientType::Viewer)]))),
        ..options(true)
    });
    Sock::join(
        &registry,
        None,
        &[&page("controller", Some("ann-Hq7"), "tab-a", 1)],
    )
    .await;
    let uid = registry.peer_ids().pop().unwrap();
    assert_eq!(
        registry.peer(&uid).unwrap().client_type,
        Some(ClientType::Viewer)
    );
}

#[tokio::test]
async fn without_a_master_token_a_newer_page_of_a_slot_supersedes_the_old() {
    let registry = PeerRegistry::new(options(true));
    let first = Sock::join(&registry, None, &[&page("viewer", None, "tab-1", 1)]).await;
    let second = Sock::join(&registry, None, &[&page("viewer", None, "tab-2", 1)]).await;
    assert_eq!(first.close_code(), Some(4001));
    assert!(!second.closed());
}

#[tokio::test]
async fn a_controller_of_another_tab_joins_beside_with_sharing_on() {
    let registry = PeerRegistry::new(options(true));
    let first = Sock::join(&registry, None, &[&page("controller", None, "tab-1", -1)]).await;
    let second = Sock::join(&registry, None, &[&page("controller", None, "tab-2", -1)]).await;
    assert!(!first.closed() && !second.closed());
    let again = Sock::join(&registry, None, &[&page("controller", None, "tab-1", -1)]).await;
    assert_eq!(
        first.close_code(),
        Some(4001),
        "its own tab reconnecting supersedes it"
    );
    assert!(!again.closed() && !second.closed());
}

#[tokio::test]
async fn a_secondary_displays_viewer_needs_its_controller() {
    let registry = PeerRegistry::new(options(true));
    let line =
        hello_json(json!({"client_type": "viewer", "client_slot": -1, "display_id": "display2"}));
    let refused = Sock::join(&registry, None, &[&line]).await;
    let (code, reason) = refused.close_args().expect("closed");
    assert_eq!(code, 4000);
    assert!(reason.contains("controller"), "{reason}");
    let lone = Sock::join(&registry, None, &[&hello("viewer", -1)]).await;
    assert!(
        !lone.closed(),
        "a viewer of the primary needs no controller"
    );
}

#[tokio::test]
async fn strict_shared_clients_are_refused_when_shared_is_off() {
    let registry = PeerRegistry::new(SignalingOptions {
        enable_shared: false,
        ..options(true)
    });
    let line = hello_json(
        json!({"client_type": "viewer", "client_slot": -1, "client_strict_viewer": true}),
    );
    assert_eq!(
        Sock::join(&registry, None, &[&line]).await.close_code(),
        Some(4000)
    );
}

#[tokio::test]
async fn client_presence_follows_the_browser_peers_alone() {
    let registry = PeerRegistry::new(options(true));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    registry.set_on_client_presence(Arc::new(move |present| seen.lock().unwrap().push(present)));
    let server_hello = format!(
        "HELLO server {}",
        json!({"server_token": registry.server_peer_secret()})
    );
    let _server = Sock::join(&registry, None, &[&server_hello]).await;
    assert_eq!(calls.lock().unwrap().last(), Some(&false));
    let page = Sock::join(&registry, None, &[&hello("viewer", -1)]).await;
    assert_eq!(calls.lock().unwrap().last(), Some(&true));
    page.close_frame().await;
    assert_eq!(calls.lock().unwrap().last(), Some(&false));
}

#[tokio::test]
async fn dropping_a_connections_service_ends_its_peer() {
    let registry = PeerRegistry::new(options(true));
    let page = Sock::join(&registry, None, &[&hello("viewer", -1)]).await;
    assert_eq!(registry.peer_ids().len(), 1);
    page.task.abort();
    settle().await;
    assert!(registry.peer_ids().is_empty());
}

#[tokio::test]
async fn a_page_that_stops_reading_is_cut_not_grown() {
    let (peer_socket, _outbox) = socket();
    let line = "x".repeat(1024 * 1024);
    let mut taken = 0;
    while peer_socket.send_text(line.clone()).is_ok() {
        taken += 1;
        assert!(taken < 100, "the queue is bounded");
    }
    assert!(peer_socket.is_closed());
    assert!(peer_socket.was_aborted());
    assert_eq!(taken, super::OUTBOX_MAX_BYTES / line.len());
}

// The server peer's client

#[tokio::test]
async fn the_in_process_client_registers_and_carries_a_session() {
    let registry = PeerRegistry::new(options(true));
    let connector = Arc::new(RegistryConnector(Arc::clone(&registry)));
    let (client, mut events) =
        SignalingClient::spawn(connector, Some(registry.server_peer_secret().to_owned()));
    settle().await;
    assert!(client.send_sdp("offer", "x", "nobody").is_ok(), "connected");

    let meta =
        json!({"client_type": "controller", "client_slot": -1, "fullcolor_codecs": ["h264"]});
    let page = Sock::join(&registry, None, &[&hello_json(meta), "SESSION server"]).await;
    let Some(ClientEvent::SessionStart(start)) = events.recv().await else {
        panic!("a session start");
    };
    assert_eq!(start.fullcolor_codecs, Some(vec!["h264".to_owned()]));

    client.send_sdp("offer", "v=0\r\n", &start.peer_id).unwrap();
    settle().await;
    let line = page.sent().last().cloned().unwrap();
    let (from, body) = line.split_once(' ').unwrap();
    assert!(from.starts_with("server-"));
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({"sdp": {"type": "offer", "sdp": "v=0\r\n"}})
    );

    client
        .send_ice(1, "candidate:1 1 udp 1 1.2.3.4 5 typ host", &start.peer_id)
        .unwrap();
    settle().await;
    let line = page.sent().last().cloned().unwrap();
    let body = line.split_once(' ').unwrap().1;
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        json!({"ice": {"candidate": "candidate:1 1 udp 1 1.2.3.4 5 typ host", "sdpMLineIndex": 1}})
    );

    page.send(&format!(
        r#"{from} {{"sdp": {{"type": "answer", "sdp": "v=0"}}}}"#
    ))
    .await;
    assert_eq!(
        events.recv().await,
        Some(ClientEvent::Sdp {
            peer_id: start.peer_id.clone(),
            kind: "answer".to_owned(),
            sdp: "v=0".to_owned()
        })
    );
    page.send(&format!(
        r#"{from} {{"ice": {{"candidate": "", "sdpMid": "0"}}}}"#
    ))
    .await;
    let Some(ClientEvent::Ice { ice, .. }) = events.recv().await else {
        panic!("an ICE message");
    };
    assert_eq!(ice.to_candidate(), Ok(None), "the end-of-candidates marker");
    page.send(&format!("{from} not json")).await;
    page.send(&format!("{from} [1]")).await;
    page.close_frame().await;
    assert_eq!(
        events.recv().await,
        Some(ClientEvent::SessionEnd {
            peer_id: start.peer_id.clone(),
            client_type: "controller".to_owned()
        }),
        "the malformed lines were dropped, the connection kept"
    );
    client.stop();
}

#[tokio::test]
async fn the_client_reconnects_when_its_connection_ends() {
    let registry = PeerRegistry::new(options(true));
    let connector = Arc::new(RegistryConnector(Arc::clone(&registry)));
    let (client, mut events) =
        SignalingClient::spawn(connector, Some(registry.server_peer_secret().to_owned()));
    settle().await;
    let server_uid = registry.peer_ids().pop().expect("registered");
    registry.close_peer(&server_uid, 1000, "test");
    assert_eq!(events.recv().await, Some(ClientEvent::Disconnected));
    tokio::time::sleep(super::client::RECONNECT_AFTER_END * 2).await;
    let peers = registry.peer_ids();
    assert_eq!(peers.len(), 1);
    assert_ne!(peers[0], server_uid);
    client.stop();
    settle().await;
    assert!(matches!(
        client.send_sdp("offer", "x", "y"),
        Err(super::SignalingError::NotConnected)
    ));
}

// A display's graph

#[test]
fn a_displays_graph_follows_its_consumers() {
    use ClientType::{Controller, Viewer};
    let mut graphs = DisplayGraphs::default();
    assert_eq!(
        graphs.admit(Viewer, "primary"),
        Ok(true),
        "a lone viewer builds the primary"
    );
    graphs.register("v1", Viewer, "primary");
    assert_eq!(
        graphs.admit(Controller, "primary"),
        Ok(false),
        "a controller joining reuses it"
    );
    graphs.register("c1", Controller, "primary");
    assert_eq!(
        graphs.leave("c1"),
        Release::default(),
        "the viewer keeps graph and capture"
    );
    assert!(graphs.has_graph("primary"));
    let release = graphs.leave("v1");
    assert_eq!(
        release.stop_display.as_deref(),
        Some("primary"),
        "released with the last consumer"
    );
    assert!(!graphs.has_graph("primary"));

    assert_eq!(graphs.admit(Controller, "primary"), Ok(true));
    graphs.register("c1", Controller, "primary");
    assert_eq!(graphs.admit(Controller, "display2"), Ok(true));
    graphs.register("c2", Controller, "display2");
    assert_eq!(graphs.admit(Viewer, "display2"), Ok(false));
    graphs.register("v2", Viewer, "display2");
    let release = graphs.leave("c2");
    assert_eq!(
        release.stop_display.as_deref(),
        Some("display2"),
        "its controller leaving ends it"
    );
    assert_eq!(
        release.close_viewers,
        vec!["v2".to_owned()],
        "its viewer is closed with it"
    );
    assert_eq!(
        graphs.leave("v2"),
        Release::default(),
        "no release churn as that viewer is reaped"
    );
    assert!(
        graphs.has_graph("primary") && graphs.has_peer("c1"),
        "the primary is untouched"
    );

    assert!(
        graphs.admit(Viewer, "display2").is_err(),
        "a secondary viewer without a controller is refused"
    );

    assert_eq!(graphs.admit(Controller, "display2"), Ok(true));
    let release = graphs.failed_start(Controller, "display2");
    assert_eq!(
        release.stop_display.as_deref(),
        Some("display2"),
        "a failed start leaves nothing"
    );
    assert!(!graphs.has_graph("display2"));

    assert_eq!(graphs.admit(Controller, "primary"), Ok(false));
    assert_eq!(
        graphs.failed_start(Controller, "primary"),
        Release::default(),
        "c1 still consumes it"
    );
    assert!(graphs.has_graph("primary"));
}

// The reconnect grace

fn counted(count: &Arc<AtomicUsize>) -> impl FnOnce() -> std::future::Ready<()> + Send + 'static {
    let count = Arc::clone(count);
    move || {
        count.fetch_add(1, Ordering::SeqCst);
        std::future::ready(())
    }
}

#[tokio::test]
async fn the_primary_capture_survives_a_reload_within_the_grace() {
    let grace = Duration::from_millis(60);
    let stops = Arc::new(AtomicUsize::new(0));

    let pending = StopGrace::new();
    pending.schedule(grace, || false, counted(&stops));
    assert!(
        pending.is_pending() && stops.load(Ordering::SeqCst) == 0,
        "deferred, not immediate"
    );
    tokio::time::sleep(grace / 2).await;
    pending.cancel();
    assert!(
        !pending.is_pending(),
        "a consumer reclaiming cancels the pending stop"
    );
    tokio::time::sleep(grace).await;
    assert_eq!(stops.load(Ordering::SeqCst), 0, "the capture stays warm");

    pending.schedule(grace, || false, counted(&stops));
    pending.schedule(grace, || false, counted(&stops));
    assert_eq!(
        stops.load(Ordering::SeqCst),
        0,
        "still running within the grace"
    );
    tokio::time::sleep(grace * 2).await;
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "nobody reclaimed: stopped once"
    );

    pending.schedule(grace, || true, counted(&stops));
    tokio::time::sleep(grace * 2).await;
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "a consumer present at expiry keeps the capture"
    );
}
