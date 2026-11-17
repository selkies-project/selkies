/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a websockets client is told, and what it may do, at connect.
//!
//! mk-access: in secure mode every tokened client receives its input-authority verdict in the
//! handshake (MK_ACCESS,1 for the mk-token holder, a controller while no mk token is provisioned,
//! MK_ACCESS,0 for everyone else) after the MODE message that makes the page build the input
//! context the verdict applies to, the way WebRTC pushes it at channel open. A viewer holding the
//! mk token stays read-only with collab disabled. The session token never reaches the server log.
//!
//! no-resize: with dynamic resizing disabled nothing a SETTINGS carries resizes the desktop an
//! operator sized with `selkies resize`: not the page's window in the first one, not 16-pixel
//! alignment, and not a manual resolution in the first or a later one. The server keeps the
//! desktop's current size and tells the client the realized geometry to fit. With it enabled the
//! same SETTINGS does resize, which is what proves the check can see one.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use selkies_suites::server::test_display;
use selkies_suites::x11::root_size;
use selkies_suites::{Checks, Server, http, server, server_binary};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_websockets::{ClientBuilder, Limits, Message};

const MASTER: &str = "e2e-master-token";
const CONTROL_TOKEN: &str = "e2e-ctrl-Qx9";
const VIEW_TOKEN: &str = "e2e-view-Zt4";

/// Replace the session token table through the Bearer-gated endpoint; its status, 0 on no answer.
async fn post_tokens(server: &Server, table: &Value) -> u16 {
    let bearer = format!("Bearer {MASTER}");
    http::request(
        server.port(),
        "POST",
        "/api/tokens",
        &[
            ("Authorization", &bearer),
            ("Content-Type", "application/json"),
        ],
        table.to_string().as_bytes(),
        Duration::from_secs(10),
    )
    .await
    .map_or(0, |r| r.status)
}

/// The text messages of a connect's first `seconds`, and the close code when the server closed it
/// in that time.
async fn handshake(server: &Server, query: &str, seconds: f64) -> (Vec<String>, Option<u16>) {
    let uri = server.websocket(query);
    let mut messages = Vec::new();
    let connected = async {
        ClientBuilder::new()
            .uri(&uri)
            .map_err(|e| e.to_string())?
            .limits(Limits::unlimited())
            .connect()
            .await
            .map_err(|e| e.to_string())
    };
    let mut socket = match connected.await {
        Ok((socket, _)) => socket,
        Err(e) => {
            messages.push(format!("ERROR {e}"));
            return (messages, None);
        }
    };
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut code = None;
    while let Ok(next) = tokio::time::timeout_at(deadline, socket.next()).await {
        match next {
            Some(Ok(message)) => {
                if let Some(text) = message.as_text() {
                    messages.push(text.to_owned());
                } else if let Some((close, _)) = message.as_close() {
                    code = Some(u16::from(close));
                }
            }
            _ => break,
        }
    }
    let _ = socket.close().await;
    (messages, code)
}

/// The AUTH_SUCCESS role, the MK_ACCESS verdict, and whether MODE preceded it.
fn verdict(messages: &[String]) -> (Option<String>, Option<String>, bool) {
    let (mut role, mut mk, mut mode_at, mut mk_at) = (None, None, None, None);
    for (i, m) in messages.iter().enumerate() {
        if let Some(payload) = m.strip_prefix("AUTH_SUCCESS,") {
            role = Some(
                serde_json::from_str::<Value>(payload)
                    .ok()
                    .and_then(|v| v.get("role").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_else(|| "unparsable".to_owned()),
            );
        } else if m.starts_with("MODE ") && mode_at.is_none() {
            mode_at = Some(i);
        } else if let Some(value) = m.strip_prefix("MK_ACCESS,")
            && mk_at.is_none()
        {
            mk_at = Some(i);
            mk = Some(value.trim().to_owned());
        }
    }
    let ordered = matches!((mode_at, mk_at), (Some(a), Some(b)) if a < b);
    (role, mk, ordered)
}

/// The first four messages, for a check's detail.
fn first(messages: &[String]) -> String {
    format!("{:?}", &messages[..messages.len().min(4)])
}

fn is(value: &Option<String>, want: &str) -> bool {
    value.as_deref() == Some(want)
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn mk_access() {
    let mut res = Checks::new("mk-access");
    let mut logs = String::new();
    let server = server::launch("mk-access")
        .env("SELKIES_MASTER_TOKEN", MASTER)
        .start()
        .await;
    let status = post_tokens(
        &server,
        &json!({
            CONTROL_TOKEN: {"role": "controller", "slot": 1},
            VIEW_TOKEN: {"role": "viewer", "slot": null, "mk_control": true},
        }),
    )
    .await;
    res.check(
        "tokens provisioned (viewer holds mk)",
        status == 200,
        status,
    );
    let pause = || tokio::time::sleep(Duration::from_secs(1));

    let (msgs, _) = handshake(&server, &format!("?token={VIEW_TOKEN}"), 4.0).await;
    let (role, mk, ordered) = verdict(&msgs);
    res.check(
        "viewer holding mk: AUTH_SUCCESS names the viewer role",
        is(&role, "viewer"),
        first(&msgs),
    );
    res.check(
        "viewer holding mk: MK_ACCESS,1 in the handshake",
        is(&mk, "1"),
        first(&msgs),
    );
    res.check(
        "viewer holding mk: the verdict follows MODE",
        ordered,
        first(&msgs),
    );
    pause().await;

    let (msgs, _) = handshake(&server, &format!("?token={CONTROL_TOKEN}"), 4.0).await;
    let (role, mk, ordered) = verdict(&msgs);
    res.check(
        "controller outranked by the mk token: MK_ACCESS,0",
        is(&role, "controller") && is(&mk, "0") && ordered,
        first(&msgs),
    );
    pause().await;

    let status = post_tokens(
        &server,
        &json!({
            CONTROL_TOKEN: {"role": "controller", "slot": 1},
            VIEW_TOKEN: {"role": "viewer", "slot": null},
        }),
    )
    .await;
    res.check("tokens re-provisioned (no mk token)", status == 200, status);
    let (msgs, _) = handshake(&server, &format!("?token={CONTROL_TOKEN}"), 4.0).await;
    let (role, mk, ordered) = verdict(&msgs);
    res.check(
        "no mk token: a controller connects with MK_ACCESS,1",
        is(&role, "controller") && is(&mk, "1") && ordered,
        first(&msgs),
    );
    pause().await;
    let (msgs, _) = handshake(&server, &format!("?token={VIEW_TOKEN}"), 4.0).await;
    let (role, mk, ordered) = verdict(&msgs);
    res.check(
        "no mk token: a viewer connects with MK_ACCESS,0",
        is(&role, "viewer") && is(&mk, "0") && ordered,
        first(&msgs),
    );
    pause().await;

    let prefix = &VIEW_TOKEN[..VIEW_TOKEN.len() - 1];
    let (msgs, code) = handshake(&server, &format!("?token={prefix}"), 2.0).await;
    res.check(
        "a token that is only a prefix of a provisioned one is refused",
        code == Some(4001) && !msgs.iter().any(|m| m.starts_with("AUTH_SUCCESS")),
        format!("close {code:?} {:?}", &msgs[..msgs.len().min(2)]),
    );
    let log = server.log();
    res.check(
        "session tokens never reach the server log",
        !log.contains(CONTROL_TOKEN) && !log.contains(VIEW_TOKEN) && !log.contains(prefix),
        "",
    );
    logs.push_str(&server.stop().await);

    let server = server::launch("mk-access-collab-off")
        .env("SELKIES_MASTER_TOKEN", MASTER)
        .env("SELKIES_ENABLE_COLLAB", "false")
        .start()
        .await;
    let status = post_tokens(
        &server,
        &json!({
            CONTROL_TOKEN: {"role": "controller", "slot": 1},
            VIEW_TOKEN: {"role": "viewer", "slot": null, "mk_control": true},
        }),
    )
    .await;
    res.check(
        "tokens provisioned with collab disabled",
        status == 200,
        status,
    );
    let (msgs, _) = handshake(&server, &format!("?token={VIEW_TOKEN}"), 4.0).await;
    let (role, mk, ordered) = verdict(&msgs);
    res.check(
        "collab off: a viewer holding mk is told MK_ACCESS,0",
        is(&role, "viewer") && is(&mk, "0") && ordered,
        first(&msgs),
    );
    logs.push_str(&server.stop().await);
    res.finish(&logs);
}

/// The desktop a pinned server keeps: a laptop panel, which 16-pixel alignment would cut to
/// 1920x1072.
const PANEL: (u16, u16) = (1920, 1080);
/// What the page asks for, aligned, so alignment alone never moves it.
const WANT: (u16, u16) = (1280, 720);

fn settings_payload(width: u16, height: u16, extra: &Value) -> Value {
    let mut payload = json!({
        "displayId": "primary", "initialClientWidth": width, "initialClientHeight": height,
        "manual_resolution": false, "framerate": 30, "encoder": "jpeg",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    });
    if let (Some(base), Some(more)) = (payload.as_object_mut(), extra.as_object()) {
        base.extend(more.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    payload
}

/// A page in manual mode: the manual size in place of its window's.
fn manual_payload(width: u16, height: u16) -> Value {
    settings_payload(
        width,
        height,
        &json!({"manual_resolution": true, "manual_width": width, "manual_height": height}),
    )
}

fn root() -> (u16, u16) {
    root_size(&test_display()).unwrap_or_default()
}

/// Connect and send each SETTINGS in turn, eight seconds apart so the capture runs and any
/// reconfigure lands before the root is read: the first stream_resolution payload, and the root
/// size read after each SETTINGS.
async fn send_settings(server: &Server, payloads: &[Value]) -> (Option<Value>, Vec<(u16, u16)>) {
    let uri = server.websocket("");
    let (mut socket, _) = ClientBuilder::new()
        .uri(&uri)
        .expect("a WebSocket address")
        .limits(Limits::unlimited())
        .connect()
        .await
        .unwrap_or_else(|e| panic!("{uri} refused the socket: {e}\n{}", server.log_tail()));
    let _ = tokio::time::timeout(Duration::from_secs(10), socket.next()).await;
    let mut resolution = None;
    let mut roots = Vec::new();
    for payload in payloads {
        let _ = socket
            .send(Message::text(format!("SETTINGS,{payload}")))
            .await;
        let deadline = Instant::now() + Duration::from_secs(8);
        while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, socket.next()).await {
            let Some(text) = message.as_text() else {
                continue;
            };
            if resolution.is_none()
                && text.starts_with('{')
                && let Ok(reply) = serde_json::from_str::<Value>(text)
                && reply.get("type").and_then(Value::as_str) == Some("stream_resolution")
            {
                resolution = Some(reply);
            }
        }
        // A socket the server closed reads nothing more; the root is still read.
        tokio::time::sleep_until(deadline).await;
        roots.push(root());
    }
    let _ = socket.send(Message::text("STOP_VIDEO")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = socket.close().await;
    (resolution, roots)
}

/// Size the test display as an operator sizes a desktop Selkies may not resize
/// (`selkies resize`, docs/native.md); the root size realized.
fn resize_desktop((width, height): (u16, u16)) -> (u16, u16) {
    let _ = std::process::Command::new(server_binary())
        .args(["resize", &format!("{width}x{height}")])
        .env("DISPLAY", test_display())
        .output();
    root()
}

fn size(value: &Option<Value>) -> Option<(u64, u64)> {
    let v = value.as_ref()?;
    Some((v.get("width")?.as_u64()?, v.get("height")?.as_u64()?))
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn no_resize() {
    let mut res = Checks::new("no-resize");
    let mut logs = String::new();
    let start = root();
    let root_now = resize_desktop(PANEL);
    res.check(
        "the desktop is sized to the panel",
        root_now == PANEL,
        format!("{root_now:?}"),
    );

    let server = server::launch("no-resize")
        .env("SELKIES_ENABLE_RESIZE", "false")
        .start()
        .await;
    let (resolution, roots) = send_settings(
        &server,
        &[
            settings_payload(WANT.0, WANT.1, &json!({})),
            settings_payload(WANT.0, WANT.1, &json!({"force_aligned_resolution": true})),
            manual_payload(WANT.0, WANT.1),
        ],
    )
    .await;
    res.check(
        "resize disabled: the desktop keeps its size on first SETTINGS",
        roots[0] == root_now,
        format!("root {root_now:?} -> {:?}", roots[0]),
    );
    let realized = (u64::from(root_now.0), u64::from(root_now.1));
    res.check(
        "resize disabled: the client is told the realized geometry",
        size(&resolution) == Some(realized),
        format!("{resolution:?}"),
    );
    res.check(
        "resize disabled: 16-pixel alignment keeps the desktop's size",
        roots[1] == root_now,
        format!("root {root_now:?} -> {:?}", roots[1]),
    );
    res.check(
        "resize disabled: a manual resolution keeps the desktop's size",
        roots[2] == root_now,
        format!("root {root_now:?} -> {:?}, asked {WANT:?}", roots[2]),
    );
    resize_desktop(root_now);
    let (_, roots) = send_settings(&server, &[manual_payload(WANT.0, WANT.1)]).await;
    res.check(
        "resize disabled: a page connecting in manual mode keeps the desktop's size",
        roots[0] == root_now,
        format!("root {root_now:?} -> {:?}, asked {WANT:?}", roots[0]),
    );
    let log = server.log();
    res.check(
        "resize disabled: the server logs the ignored initial size",
        log.contains("dynamic resizing disabled"),
        "",
    );
    res.check(
        "resize disabled: the capture still starts",
        log.contains("Capture started for 'primary'"),
        "",
    );
    logs.push_str(&server.stop().await);

    let server = server::launch("resize")
        .env("SELKIES_ENABLE_RESIZE", "true")
        .start()
        .await;
    let (_, roots) = send_settings(&server, &[settings_payload(WANT.0, WANT.1, &json!({}))]).await;
    res.check(
        "resize enabled: the same SETTINGS resizes the desktop",
        roots[0] == WANT,
        format!("root {root_now:?} -> {:?}, wanted {WANT:?}", roots[0]),
    );
    // Put the shared display back the way it was found, once no server is left to re-apply its
    // own layout on the way out.
    logs.push_str(&server.stop().await);
    let restored = resize_desktop(start);
    res.check(
        "display restored",
        restored == start,
        format!("{restored:?}"),
    );
    res.finish(&logs);
}
