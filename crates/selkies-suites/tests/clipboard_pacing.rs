/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Audio while a large clipboard crosses the session socket.
//!
//! Audio, video, input, and the clipboard share one ordered connection, so a transfer written as
//! fast as the socket accepts it leaves megabytes in front of the next audio packet -- and the
//! socket buffer absorbs them without the application ever seeing backpressure, so on a real
//! link the stream stops for as long as the buffer takes to drain. The measurement needs a link
//! that is not loopback: a metered relay stands in for one, holding no more than that link
//! would, and the check is the worst gap between audio frames while the transfer runs.
//!
//! The `bloat` case gives that relay a deep buffer instead, the shape a reverse proxy in front of
//! the server has. The sender's own queue then stays empty however fast it writes, so a transfer
//! paced by that queue alone runs flat out and the stream waits behind the whole buffer; only a
//! gauge measuring the far end -- the pong queued behind the same bytes -- sees it filling.
//!
//! A raw client, so what is measured is the connection's own delivery rather than a browser's
//! jitter buffer smoothing it over. It answers the gauge's pings in the WebSocket library, as a
//! browser answers them in its network process.

use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use selkies_suites::x11::SelectionOwner;
use selkies_suites::{Checks, Client, private_dir, server};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// The link the relay meters, in bytes per second. Slow enough that a transfer left unpaced is
/// unmistakable, fast enough to carry the stream itself.
const LINK_BYTES_PER_S: f64 = 1_250_000.0;
/// Socket buffers for the relay's own two connections, about 25 ms of the link in flight.
/// Loopback autotunes to megabytes, and a relay holding one meters no link: the sender's queue
/// stays empty however fast it writes, so the gap below would be that buffer draining, as deep
/// as the host's `tcp_rmem` allows.
const RELAY_BUFFER_BYTES: i32 = 32 * 1024;
/// The `bloat` case's buffer instead: about a second of the link, the depth loopback
/// autotuning reaches here and a proxy in front would hold.
const RELAY_BLOAT_BYTES: i32 = 512 * 1024;
const CLIPBOARD_MB: usize = 8;
/// What a jitter buffer of a few 10 ms packets absorbs. An unpaced transfer overruns this by two
/// orders of magnitude, so the bound needs no margin.
const MAX_GAP: Duration = Duration::from_millis(500);

fn settings() -> String {
    format!(
        "SETTINGS,{}",
        json!({
            "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": false, "encoder": "jpeg", "framerate": 30,
            "video_crf": 25, "video_bitrate": 4000, "audio_bitrate": 128_000,
            "scaling_dpi": 96, "displayPosition": "right",
        })
    )
}

/// Lock a socket's receive and send buffers to `size`.
fn meter(stream: &TcpStream, size: i32) {
    let fd = stream.as_raw_fd();
    for option in [libc::SO_RCVBUF, libc::SO_SNDBUF] {
        // SAFETY: `fd` is an open socket and `size` an int that outlives the call.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                (&raw const size).cast(),
                libc::socklen_t::try_from(size_of::<i32>()).unwrap_or(4),
            );
        }
    }
}

/// One direction of the metered link: what `from` sends reaches `to` at `rate` bytes a second,
/// a packet at a time, with no more than 50 ms of the link banked while it idles.
async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    rate: f64,
) {
    let mut buffer = [0u8; 1500];
    let mut allowance = 0.0f64;
    let mut last = Instant::now();
    while let Ok(n) = from.read(&mut buffer).await {
        if n == 0 {
            break;
        }
        let now = Instant::now();
        allowance = (allowance + now.duration_since(last).as_secs_f64() * rate).min(rate * 0.05);
        last = now;
        #[expect(clippy::cast_precision_loss, reason = "a packet's length")]
        let size = n as f64;
        allowance -= size;
        if allowance < 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(-allowance / rate)).await;
            allowance = 0.0;
            last = Instant::now();
        }
        if to.write_all(&buffer[..n]).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}

/// A metered relay to the server's port, holding `buffer` bytes on each of its sockets; its
/// address.
async fn relay(target: u16, buffer: i32) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the relay's port");
    let port = listener.local_addr().expect("the relay's address").port();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            meter(&client, buffer);
            tokio::spawn(async move {
                let Ok(upstream) = TcpStream::connect(("127.0.0.1", target)).await else {
                    return;
                };
                meter(&upstream, buffer);
                let (client_read, client_write) = client.into_split();
                let (upstream_read, upstream_write) = upstream.into_split();
                tokio::join!(
                    pump(client_read, upstream_write, LINK_BYTES_PER_S),
                    pump(upstream_read, client_write, LINK_BYTES_PER_S),
                );
            });
        }
    });
    // By literal, as the relay binds it: `localhost` can resolve to `::1` first, which the relay
    // does not listen on.
    format!("ws://127.0.0.1:{port}/api/websockets")
}

/// A tone into the capture sink, so the session has audio to stream; the module's index.
fn load_sine() -> Option<String> {
    let out = Command::new("pactl")
        .args(["load-module", "module-sine", "sink=output", "frequency=440"])
        .output()
        .ok()?;
    let index = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!index.is_empty()).then_some(index)
}

/// What holds the session clipboard: an X owner, or `wl-copy` serving it on Wayland.
enum Source {
    X(SelectionOwner),
    Wayland(std::process::Child),
}

/// Put `payload` on the session clipboard as text, as an application would.
///
/// # Panics
///
/// When `wl-copy` does not start or does not take the payload: the transfer the suite measures
/// would never run.
fn push_clipboard(wayland: bool, payload: &[u8]) -> Source {
    if !wayland {
        return Source::X(SelectionOwner::serving(&server::test_display(), payload));
    }
    let mut copy = Command::new("wl-copy")
        .args(["-t", "text/plain"])
        .env("WAYLAND_DISPLAY", "wayland-1")
        .env("XDG_RUNTIME_DIR", private_dir("run"))
        .stdin(Stdio::piped())
        .spawn()
        .expect("wl-copy starts");
    let mut stdin = copy.stdin.take().expect("wl-copy's stdin");
    stdin.write_all(payload).expect("wl-copy takes the payload");
    drop(stdin);
    Source::Wayland(copy)
}

/// The gaps between audio frames from `start` to `end`.
fn gaps(stamps: &[Instant], start: Instant, end: Instant) -> Vec<Duration> {
    let window: Vec<Instant> = stamps
        .iter()
        .copied()
        .filter(|t| *t >= start && *t <= end)
        .collect();
    window.windows(2).map(|w| w[1] - w[0]).collect()
}

fn worst(gaps: &[Duration]) -> Duration {
    gaps.iter().copied().max().unwrap_or_default()
}

async fn block(case: &str, wayland: bool, buffer: i32) {
    let mut checks = Checks::new(&format!("clippacing-{case}"));
    let module = load_sine();
    let server = server::launch(case).wayland(wayland).start().await;
    let uri = relay(server.port(), buffer).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let page = Client::connect_at(&server, &uri).await;
    page.send(&settings());
    page.send("START_AUDIO");
    tokio::time::sleep(Duration::from_secs(8)).await;
    let idle_at = Instant::now();
    tokio::time::sleep(Duration::from_secs(10)).await;
    let push_at = Instant::now();
    let block = [b"CLIP".as_slice(), &[b'x'; 4092]]
        .concat()
        .repeat(CLIPBOARD_MB * 256);
    let source = push_clipboard(wayland, &block);
    tokio::time::sleep(Duration::from_secs(25)).await;
    let done_at = Instant::now();
    let stamps: Vec<Instant> = page
        .binaries(0)
        .iter()
        .filter(|b| b.len > 0 && b.head[0] == 0x01)
        .map(|b| b.at)
        .collect();
    page.close().await;
    match source {
        Source::Wayland(mut copy) => {
            let _ = copy.kill();
            let _ = copy.wait();
        }
        Source::X(owner) => drop(owner),
    }
    let idle = gaps(&stamps, idle_at, push_at);
    let during = gaps(&stamps, push_at, done_at);
    checks.check(
        "audio streams over the metered link",
        stamps.len() > 100 && !idle.is_empty(),
        format!("{} frames", stamps.len()),
    );
    if !idle.is_empty() && !during.is_empty() {
        checks.check(
            "audio is even before the transfer",
            worst(&idle) < MAX_GAP,
            format!("worst idle gap {} ms", worst(&idle).as_millis()),
        );
        checks.check(
            "a large clipboard does not stall the audio",
            worst(&during) < MAX_GAP,
            format!(
                "worst gap {} ms during {CLIPBOARD_MB} MB (idle {} ms)",
                worst(&during).as_millis(),
                worst(&idle).as_millis()
            ),
        );
    }
    let log = server.stop().await;
    if let Some(index) = module {
        let _ = Command::new("pactl")
            .args(["unload-module", &index])
            .output();
    }
    checks.finish(&log);
}

#[tokio::test]
#[ignore = "needs a built selkies server, the X test display (E2E_DISPLAY) and a sound server"]
async fn websockets() {
    block("x11", false, RELAY_BUFFER_BYTES).await;
}

#[tokio::test]
#[ignore = "needs a built selkies server, the X test display (E2E_DISPLAY), a sound server and wl-copy"]
async fn wayland() {
    block("wl", true, RELAY_BUFFER_BYTES).await;
}

#[tokio::test]
#[ignore = "needs a built selkies server, the X test display (E2E_DISPLAY) and a sound server"]
async fn bloat() {
    block("bloat", false, RELAY_BLOAT_BYTES).await;
}
