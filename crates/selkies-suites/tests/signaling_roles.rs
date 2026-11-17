/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Who may hold the WebRTC signaling roles, outside secure mode too.
//!
//! The streaming server's own signaling client holds the server role with a proof of it; a socket
//! that says HELLO server without one is closed and the real server peer stays registered. A
//! page's SESSION only ever calls the server: one naming another page is refused, and that page
//! hears nothing of it, while the same page calling the server is answered. Raw signaling sockets
//! against a WebRTC server, no browser.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use selkies_suites::{Checks, Server, server};
use serde_json::json;
use tokio::net::TcpStream;
use tokio_websockets::{ClientBuilder, MaybeTlsStream, Message, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The `peer_type` peers the server has logged as registered, in order: each
/// `Registered peer <peer_type>-<[0-9a-f-]+>`.
fn registered(server: &Server, peer_type: &str) -> Vec<String> {
    let log = server.log();
    let marker = format!("Registered peer {peer_type}-");
    log.match_indices(&marker)
        .filter_map(|(at, _)| {
            let uid: String = log[at + marker.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || ('a'..='f').contains(c) || *c == '-')
                .collect();
            (!uid.is_empty()).then(|| format!("{peer_type}-{uid}"))
        })
        .collect()
}

async fn connect(server: &Server) -> Socket {
    let uri = format!("ws://127.0.0.1:{}/api/ws", server.port());
    ClientBuilder::new()
        .uri(&uri)
        .expect("a WebSocket address")
        .connect()
        .await
        .unwrap_or_else(|e| panic!("{uri} refused the socket: {e}\n{}", server.log_tail()))
        .0
}

/// The next text message within `wait`, `None` when none came or the socket closed.
async fn next_text(socket: &mut Socket, wait: Duration) -> Option<String> {
    loop {
        match tokio::time::timeout(wait, socket.next()).await {
            Ok(Some(Ok(message))) => {
                if let Some(text) = message.as_text() {
                    return Some(text.to_owned());
                }
                if message.is_close() {
                    return None;
                }
            }
            _ => return None,
        }
    }
}

/// A peer that said HELLO and was answered.
///
/// # Panics
///
/// When the HELLO is not taken or not answered within five seconds.
async fn hello(server: &Server, peer_type: &str, meta: &serde_json::Value) -> Socket {
    let mut socket = connect(server).await;
    socket
        .send(Message::text(format!("HELLO {peer_type} {meta}")))
        .await
        .unwrap_or_else(|e| panic!("HELLO {peer_type} was not sent: {e}"));
    assert!(
        next_text(&mut socket, Duration::from_secs(5))
            .await
            .is_some(),
        "HELLO {peer_type} was not answered\n{}",
        server.log_tail()
    );
    socket
}

/// Everything a socket hears until it is quiet for `wait`.
async fn quiet(socket: &mut Socket, wait: Duration) -> Vec<String> {
    let mut heard = Vec::new();
    while let Some(text) = next_text(socket, wait).await {
        heard.push(text);
    }
    heard
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn signaling_roles() {
    let mut res = Checks::new("signaling-roles");
    let server = server::launch("signaling-roles")
        .mode("webrtc")
        .start()
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while registered(&server, "server").is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let servers = registered(&server, "server");
    res.check(
        "the streaming server registers its server peer",
        servers.len() == 1,
        format!("{servers:?}"),
    );

    let mut code = None;
    let mut socket = connect(&server).await;
    let _ = socket.send(Message::text("HELLO server")).await;
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(Ok(message))) = tokio::time::timeout_at(until, socket.next()).await {
        if let Some((close, _)) = message.as_close() {
            code = Some(u16::from(close));
            break;
        }
    }
    drop(socket);
    res.check(
        "a HELLO server without a proof is closed with 4001",
        code == Some(4001),
        format!("{code:?}"),
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    let now = registered(&server, "server");
    res.check(
        "and the streaming server's peer stays the only one",
        now == servers,
        format!("{now:?}"),
    );

    let mut victim = hello(
        &server,
        "client",
        &json!({"client_type": "controller", "client_slot": -1}),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while registered(&server, "client").is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let victim_uid = registered(&server, "client")
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("no client peer registered\n{}", server.log_tail()));
    let mut other = hello(
        &server,
        "client",
        &json!({"client_type": "viewer", "client_slot": -1}),
    )
    .await;
    let _ = other
        .send(Message::text(format!("SESSION {victim_uid}")))
        .await;
    let reply = next_text(&mut other, Duration::from_secs(5))
        .await
        .unwrap_or_default();
    res.check(
        "a SESSION naming another page is refused",
        reply.starts_with("ERROR"),
        &reply,
    );
    let heard = quiet(&mut victim, Duration::from_millis(1500)).await;
    res.check(
        "and that page hears nothing of it",
        heard.is_empty(),
        format!("{:?}", &heard[..heard.len().min(2)]),
    );
    let _ = other.send(Message::text("SESSION server")).await;
    let reply = next_text(&mut other, Duration::from_secs(5))
        .await
        .unwrap_or_default();
    let want = format!(
        "SESSION_OK {}",
        servers.first().cloned().unwrap_or_default()
    );
    res.check(
        "the same page calling the server is answered",
        reply == want,
        &reply,
    );
    let _ = other.close().await;
    let _ = victim.close().await;
    let logs = server.stop().await;
    res.finish(&logs);
}
