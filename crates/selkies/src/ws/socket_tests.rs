/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `GET /api/websockets` over a real TCP socket with a hand-written client: the status codes,
//! the upgrade, and the frames byte for byte as a browser reads them.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::ws::testkit::*;

async fn serve(rig: &Rig) -> std::net::SocketAddr {
    let router: axum::Router = crate::ws::routes(rig.service.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

/// A client's frame: masked, as a server requires.
fn masked(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [1u8, 2, 3, 4];
    let mut out = vec![0x80 | opcode];
    if payload.len() < 126 {
        out.push(0x80 | payload.len() as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

struct Socket {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl Socket {
    async fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 65536];
        match tokio::time::timeout(Duration::from_secs(5), self.stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => {
                self.buffer.extend_from_slice(&chunk[..n]);
                true
            }
            _ => false,
        }
    }

    /// One server frame: its opcode and payload.
    async fn frame(&mut self) -> Option<(u8, Vec<u8>)> {
        loop {
            if self.buffer.len() >= 2 {
                let opcode = self.buffer[0] & 0x0F;
                let short = (self.buffer[1] & 0x7F) as usize;
                let (head, len) = match short {
                    126 if self.buffer.len() >= 4 => (
                        4,
                        u16::from_be_bytes([self.buffer[2], self.buffer[3]]) as usize,
                    ),
                    127 if self.buffer.len() >= 10 => {
                        let mut raw = [0u8; 8];
                        raw.copy_from_slice(&self.buffer[2..10]);
                        (10, u64::from_be_bytes(raw) as usize)
                    }
                    126 | 127 => (usize::MAX, 0),
                    n => (2, n),
                };
                if head != usize::MAX && self.buffer.len() >= head + len {
                    let payload = self.buffer[head..head + len].to_vec();
                    self.buffer.drain(..head + len);
                    return Some((opcode, payload));
                }
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    /// The next text frame, skipping what else comes.
    async fn text(&mut self) -> Option<String> {
        loop {
            let (opcode, payload) = self.frame().await?;
            if opcode == 1 {
                return String::from_utf8(payload).ok();
            }
        }
    }

    async fn send(&mut self, opcode: u8, payload: &[u8]) {
        self.stream
            .write_all(&masked(opcode, payload))
            .await
            .unwrap();
    }
}

async fn request(addr: std::net::SocketAddr, headers: &str) -> (String, TcpStream, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET /api/websockets HTTP/1.1\r\nHost: x\r\n{headers}\r\n").as_bytes())
        .await
        .unwrap();
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    let end = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(buffer.len());
    let head = String::from_utf8_lossy(&buffer[..end]).to_string();
    let rest = buffer[end..].to_vec();
    (head, stream, rest)
}

const UPGRADE: &str = "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: selkies\r\n";

#[tokio::test]
async fn a_plain_get_is_the_reconnect_probe() {
    let rig = Rig::new();
    let addr = serve(&rig).await;
    let (head, ..) = request(addr, "").await;
    assert!(head.starts_with("HTTP/1.1 204"), "{head}");
}

#[tokio::test]
async fn secure_mode_wants_a_token_on_the_upgrade() {
    let rig = Rig::new();
    *rig.tokens.table.lock() = Some(vec![]);
    let addr = serve(&rig).await;
    let (head, ..) = request(addr, UPGRADE).await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
    let (head, ..) = request(
        addr,
        "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: selkies.token.abc\r\n",
    )
    .await;
    assert!(
        head.starts_with("HTTP/1.1 400"),
        "a token subprotocol without selkies: {head}"
    );
}

#[tokio::test]
async fn a_socket_speaks_the_wire_end_to_end() {
    let rig = Rig::with_timings(crate::ws::Timings {
        reconnect_grace_s: 60.0,
        backpressure_check_s: 3600.0,
        ..Default::default()
    });
    let addr = serve(&rig).await;
    let (head, stream, rest) = request(addr, UPGRADE).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        head.to_lowercase()
            .contains("sec-websocket-protocol: selkies"),
        "{head}"
    );
    let mut socket = Socket {
        stream,
        buffer: rest,
    };
    assert_eq!(socket.text().await.as_deref(), Some("MODE websockets"));
    assert!(
        socket
            .text()
            .await
            .unwrap()
            .starts_with("DISPLAY_CONFIG_UPDATE,")
    );
    let settings = socket.text().await.unwrap();
    assert!(
        settings.contains("\"server_settings\""),
        "{}",
        &settings[..80.min(settings.len())]
    );

    socket.send(1, b"_gz,1").await;
    assert_eq!(socket.text().await.as_deref(), Some("_gz,1"));

    // A ping is answered with its payload.
    socket.send(9, b"probe").await;
    loop {
        let (opcode, payload) = socket.frame().await.expect("the pong");
        if opcode == 10 {
            assert_eq!(payload, b"probe");
            break;
        }
    }

    // A controller's settings start the capture, and a frame goes out as pixelflux framed it.
    socket
        .send(1, br#"SETTINGS,{"initialClientWidth":640,"initialClientHeight":480,"tabId":"t","encoder":"h264enc"}"#)
        .await;
    let mut started = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if rig
            .captures
            .of("primary")
            .is_some_and(|c| c.is_capturing_now())
        {
            started = true;
            break;
        }
    }
    assert!(started);
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.primary_capture().deliver(7, 7, true);
    loop {
        let (opcode, payload) = socket.frame().await.expect("the frame");
        if opcode == 2 {
            assert_eq!(&payload[..4], &[0x04, 0x11, 0, 7]);
            assert_eq!(payload.len(), 112);
            break;
        }
    }
    // Large control text reaches a gzip-capable client as one 0x05 frame.
    rig.service.send_system_action(&"x".repeat(2000), None);
    loop {
        let (opcode, payload) = socket.frame().await.expect("the gzip frame");
        if opcode == 2 && payload[0] == 0x05 {
            let text = crate::ws::protocol::inflate_gz_bounded(&payload[1..]).unwrap();
            assert!(text.starts_with("system,"), "{}", &text[..20]);
            break;
        }
    }
    // The client's close ends the handler, and a frame past the ceiling would be refused.
    socket.send(8, &1000u16.to_be_bytes()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        rig.service.reg.lock().clients.is_empty(),
        "the socket left the registry"
    );
    assert!(
        rig.service.reg.lock().owners.contains_key("primary"),
        "its display waits out the grace"
    );
}
