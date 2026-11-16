/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A link shaper relay: `LISTEN:port -> 127.0.0.1:TARGET` with a token-bucket rate cap on the
//! target-to-client direction (the downlink), so a loopback test path meets a real bottleneck
//! exactly where a home connection would. An optional second rate shapes the client-to-target
//! direction (the uplink) the same way, for upload scenarios; without it that direction passes
//! unshaped.
//!
//! Each shaped direction is one FIFO queue shared by every connection, drained at the rate: that
//! is what a metered first hop is, and why a bulk transfer delays everything else going the same
//! way. `BWRELAY_RCVBUF` sizes that queue and the target-facing socket's receive buffer; the
//! default models a shallow hop, and a fat one (a modem holding seconds of data) is what turns an
//! upload into input lag. Every second it prints what each direction carried.
//!
//! ```text
//! bwrelay LISTEN_PORT TARGET_PORT RATE_KBIT_S [UPLINK_RATE_KBIT_S]
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpSocket};
use tokio::sync::{Mutex, mpsc};

const CHUNK: usize = 65_536;
/// The bucket's grab quantum: one 64 KiB grab at single-digit Mbit/s would hold the link for
/// tens of milliseconds, which a packet-granular link never does.
const GRAIN: usize = 8192;
/// The queue and the target-facing receive buffer by default, locked rather than the kernel's
/// autotuned one (megabytes on loopback), which would hide the standing queue where no
/// sender-side congestion gauge sees it.
const RCVBUF: usize = 128 * 1024;

/// A token bucket in bytes with a quarter-second burst allowance. Takes are FIFO (the one
/// drainer of a link takes them in queue order), so concurrent connections interleave in
/// arrival order instead of racing for the balance.
struct Bucket {
    rate: f64,
    cap: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn new(rate: f64) -> Bucket {
        Bucket {
            rate,
            cap: rate * 0.25,
            tokens: rate * 0.25,
            at: Instant::now(),
        }
    }

    fn top_up(&mut self) {
        let now = Instant::now();
        self.tokens =
            (self.tokens + now.duration_since(self.at).as_secs_f64() * self.rate).min(self.cap);
        self.at = now;
    }

    /// Debit `n` bytes, sleeping off any deficit. The deficit is repaid by the next top-up's
    /// refill; zeroing it after the sleep would credit the slept interval twice.
    async fn take(&mut self, n: usize) {
        let n = n as f64;
        loop {
            self.top_up();
            if self.tokens >= n || (self.tokens > 0.0 && n <= self.cap) {
                self.tokens -= n;
                if self.tokens < 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(-self.tokens / self.rate)).await;
                }
                return;
            }
            tokio::time::sleep(Duration::from_secs_f64(
                ((n - self.tokens) / self.rate).min(0.01),
            ))
            .await;
        }
    }
}

/// A piece of one connection's bytes queued on a link, with where it goes.
type Piece = (Arc<Mutex<OwnedWriteHalf>>, Vec<u8>);

/// One direction of the modeled link: a bounded FIFO queue drained at the rate. A sender that
/// fills it stops being read, the back-pressure a full buffer applies.
struct Link {
    queue: mpsc::Sender<Piece>,
}

impl Link {
    fn start(rate_kbit: u64, depth: usize, carried: Arc<AtomicU64>) -> Link {
        let (queue, mut pieces) = mpsc::channel::<Piece>((depth / GRAIN).max(1));
        let mut bucket = Bucket::new(rate_kbit as f64 * 1000.0 / 8.0);
        tokio::spawn(async move {
            while let Some((writer, piece)) = pieces.recv().await {
                bucket.take(piece.len()).await;
                if writer.lock().await.write_all(&piece).await.is_ok() {
                    carried.fetch_add(piece.len() as u64, Ordering::Relaxed);
                }
            }
        });
        Link { queue }
    }

    /// Move one connection's bytes onto this direction until it ends.
    async fn carry(&self, mut reader: OwnedReadHalf, writer: Arc<Mutex<OwnedWriteHalf>>) {
        let mut buf = vec![0u8; CHUNK];
        while let Ok(n) = reader.read(&mut buf).await {
            if n == 0 {
                break;
            }
            for piece in buf[..n].chunks(GRAIN) {
                if self
                    .queue
                    .send((Arc::clone(&writer), piece.to_vec()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        // The other side is closed as soon as this one ends; whatever is still queued for it
        // goes nowhere, as a connection reset on the hop drops what the hop held.
        let _ = writer.lock().await.shutdown().await;
    }
}

/// Copy one direction unshaped until it ends.
async fn pass(
    mut reader: OwnedReadHalf,
    writer: Arc<Mutex<OwnedWriteHalf>>,
    carried: Arc<AtomicU64>,
) {
    let mut buf = vec![0u8; CHUNK];
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 || writer.lock().await.write_all(&buf[..n]).await.is_err() {
            break;
        }
        carried.fetch_add(n as u64, Ordering::Relaxed);
    }
    let _ = writer.lock().await.shutdown().await;
}

fn number(arg: Option<&String>, what: &str) -> Result<u64, String> {
    arg.ok_or_else(|| format!("no {what}"))?
        .parse()
        .map_err(|e| format!("{what}: {e}"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    selkies_suites::die_with_parent();
    let args: Vec<String> = std::env::args().collect();
    let listen = number(args.get(1), "listen port")?;
    let target = number(args.get(2), "target port")?;
    let down_kbit = number(args.get(3), "rate")?;
    let up_kbit = args
        .get(4)
        .map(|a| number(Some(a), "uplink rate"))
        .transpose()?
        .unwrap_or(0);
    let depth = std::env::var("BWRELAY_RCVBUF")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(RCVBUF);

    let (down_bytes, up_bytes) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let down = Arc::new(Link::start(down_kbit, depth, Arc::clone(&down_bytes)));
    let up = (up_kbit > 0).then(|| Arc::new(Link::start(up_kbit, depth, Arc::clone(&up_bytes))));
    let listener = TcpListener::bind((
        "127.0.0.1",
        u16::try_from(listen).map_err(|e| e.to_string())?,
    ))
    .await
    .map_err(|e| format!("listen on {listen}: {e}"))?;
    let up_note = if up_kbit > 0 {
        format!(", uplink cap {up_kbit} kbit/s")
    } else {
        String::new()
    };
    println!("[relay] :{listen} -> :{target} downlink cap {down_kbit} kbit/s{up_note}");

    {
        let (down_bytes, up_bytes) = (Arc::clone(&down_bytes), Arc::clone(&up_bytes));
        tokio::spawn(async move {
            let (mut last, mut up_last) = (0u64, 0u64);
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.tick().await;
            loop {
                tick.tick().await;
                let (now, up_now) = (
                    down_bytes.load(Ordering::Relaxed),
                    up_bytes.load(Ordering::Relaxed),
                );
                let up_part = if up_kbit > 0 {
                    format!(" up={:.2} Mbit/s", (up_now - up_last) as f64 * 8.0 / 1e6)
                } else {
                    String::new()
                };
                println!(
                    "[relay] down={:.2} Mbit/s total={:.1} MB{up_part}",
                    (now - last) as f64 * 8.0 / 1e6,
                    now as f64 / 1e6
                );
                (last, up_last) = (now, up_now);
            }
        });
    }

    let target = u16::try_from(target).map_err(|e| e.to_string())?;
    loop {
        let Ok((client, _)) = listener.accept().await else {
            continue;
        };
        let (down, up, up_bytes) = (Arc::clone(&down), up.clone(), Arc::clone(&up_bytes));
        tokio::spawn(async move {
            let Ok(socket) = TcpSocket::new_v4() else {
                return;
            };
            let _ = socket.set_recv_buffer_size(u32::try_from(depth).unwrap_or(u32::MAX));
            let Ok(server) = socket.connect(([127, 0, 0, 1], target).into()).await else {
                return;
            };
            if up.is_some() {
                // Bounded like the target-facing socket, so the uplink's standing queue stands in
                // the modeled path rather than in an autotuned relay buffer.
                let _ = recv_buffer(&client, depth);
            }
            let (client_r, client_w) = client.into_split();
            let (server_r, server_w) = server.into_split();
            let (client_w, server_w) = (
                Arc::new(Mutex::new(client_w)),
                Arc::new(Mutex::new(server_w)),
            );
            let upward = async {
                match &up {
                    Some(link) => link.carry(client_r, server_w).await,
                    None => pass(client_r, server_w, up_bytes).await,
                }
            };
            tokio::join!(upward, down.carry(server_r, client_w));
        });
    }
}

/// Set an accepted socket's receive buffer, which tokio leaves to the kernel.
fn recv_buffer(stream: &tokio::net::TcpStream, bytes: usize) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let size = libc::c_int::try_from(bytes).unwrap_or(libc::c_int::MAX);
    // SAFETY: a valid socket descriptor and an int option of its size.
    let done = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            std::ptr::from_ref(&size).cast(),
            libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>()).unwrap_or(4),
        )
    };
    if done == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
