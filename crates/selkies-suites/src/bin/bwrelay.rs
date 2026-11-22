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
//! Past the queue each connection's pieces are written by a task of its own (`Out`), and a
//! connection has no more than `WINDOW` of them on their way through the relay, the queue,
//! the delay behind it and that task together. A far end that reads slowly, or not at all,
//! therefore holds its own connection's bytes and stops its own sender, as its receive
//! window would, and nobody else's: a hop forwards the other flows whatever one receiver
//! does. Written from the queue's one drainer instead, a server reading an upload at half
//! the hop's rate held every other connection's bytes for over two seconds at a time, the
//! page's acks among them.
//!
//! `BWRELAY_DELAY_MS` adds a propagation delay behind each direction, one way, and
//! `BWRELAY_JITTER_MS` up to that much more per piece with the order kept: a path with a
//! round trip of its own, which a loopback relay otherwise lacks.
//!
//! A line on stdin changes a shaped direction's rate from then on (`down KBIT_S`, `up KBIT_S`),
//! the step a congested stretch and its recovery are: what is queued stays queued and drains at
//! the new rate.
//!
//! ```text
//! bwrelay LISTEN_PORT TARGET_PORT RATE_KBIT_S [UPLINK_RATE_KBIT_S]
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpSocket};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};

const CHUNK: usize = 65_536;
/// The bucket's grab quantum: one 64 KiB grab at single-digit Mbit/s would hold the link for
/// tens of milliseconds, which a packet-granular link never does.
const GRAIN: usize = 8192;
/// The queue and the target-facing receive buffer by default, locked rather than the kernel's
/// autotuned one (megabytes on loopback), which would hide the standing queue where no
/// sender-side congestion gauge sees it.
const RCVBUF: usize = 128 * 1024;
/// What one connection may have on its way through the relay, as a receive window bounds
/// what a sender has in flight: enough for the queue and for a delay line's worth of a fast
/// link, so that only a far end that stops reading is what fills it.
const WINDOW: usize = 2 * 1024 * 1024;

/// A token bucket in bytes with a quarter-second burst allowance, at the rate its link holds
/// now. Takes are FIFO (the one drainer of a link takes them in queue order), so concurrent
/// connections interleave in arrival order instead of racing for the balance.
struct Bucket {
    /// Bytes a second, which stdin may change while the link runs.
    rate: Arc<AtomicU64>,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn new(rate: Arc<AtomicU64>) -> Bucket {
        let tokens = rate.load(Ordering::Relaxed) as f64 * 0.25;
        Bucket {
            rate,
            tokens,
            at: Instant::now(),
        }
    }

    /// The rate now, in bytes a second; never zero, so a deficit is always repaid.
    fn rate(&self) -> f64 {
        (self.rate.load(Ordering::Relaxed) as f64).max(1.0)
    }

    fn top_up(&mut self) {
        let now = Instant::now();
        let rate = self.rate();
        self.tokens =
            (self.tokens + now.duration_since(self.at).as_secs_f64() * rate).min(rate * 0.25);
        self.at = now;
    }

    /// Debit `n` bytes, sleeping off any deficit. The deficit is repaid by the next top-up's
    /// refill; zeroing it after the sleep would credit the slept interval twice.
    async fn take(&mut self, n: usize) {
        let n = n as f64;
        loop {
            self.top_up();
            let rate = self.rate();
            if self.tokens >= n || (self.tokens > 0.0 && n <= rate * 0.25) {
                self.tokens -= n;
                if self.tokens < 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(-self.tokens / rate)).await;
                }
                return;
            }
            tokio::time::sleep(Duration::from_secs_f64(
                ((n - self.tokens) / rate).min(0.01),
            ))
            .await;
        }
    }
}

/// One connection's way out of the relay: its pieces, in order, written by a task of its
/// own, so that a far end slow to read them waits on nobody else's. `room` is how many of
/// its pieces may be on their way through the relay at once (`WINDOW`), each holding a
/// permit from its reader to its writer.
#[derive(Clone)]
struct Out {
    pieces: mpsc::UnboundedSender<(Vec<u8>, OwnedSemaphorePermit)>,
    room: Arc<Semaphore>,
    shut: Arc<Notify>,
}

impl Out {
    fn start(mut writer: OwnedWriteHalf, carried: Arc<AtomicU64>) -> Out {
        let (pieces, mut queued) = mpsc::unbounded_channel::<(Vec<u8>, OwnedSemaphorePermit)>();
        let shut = Arc::new(Notify::new());
        let closing = Arc::clone(&shut);
        tokio::spawn(async move {
            loop {
                let Some((piece, _permit)) = (tokio::select! {
                    biased;
                    () = closing.notified() => None,
                    piece = queued.recv() => piece,
                }) else {
                    break;
                };
                let written = tokio::select! {
                    biased;
                    () = closing.notified() => false,
                    done = writer.write_all(&piece) => done.is_ok(),
                };
                if !written {
                    break;
                }
                carried.fetch_add(piece.len() as u64, Ordering::Relaxed);
            }
            let _ = writer.shutdown().await;
        });
        Out {
            pieces,
            room: Arc::new(Semaphore::new(WINDOW / GRAIN)),
            shut,
        }
    }

    /// Hand a piece to the connection's writer; whether it is still there to take it.
    fn send(&self, piece: Vec<u8>, permit: OwnedSemaphorePermit) -> bool {
        self.pieces.send((piece, permit)).is_ok()
    }

    /// Close the connection's far side now: what is still queued for it goes nowhere, as a
    /// connection reset on the hop drops what the hop held.
    fn close(&self) {
        self.shut.notify_one();
    }
}

/// A piece of one connection's bytes queued on a link, with where it goes and its permit.
type Piece = (Out, Vec<u8>, OwnedSemaphorePermit);

/// A propagation delay behind a direction (`BWRELAY_DELAY_MS` one way, with up to
/// `BWRELAY_JITTER_MS` more per piece, order kept): pieces leave it that long after entering.
#[derive(Clone)]
struct DelayLine {
    queue: mpsc::UnboundedSender<(Instant, Piece)>,
    delay: Duration,
    jitter_us: u64,
    seed: Arc<AtomicU64>,
}

impl DelayLine {
    fn from_env() -> Option<DelayLine> {
        let ms = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        let (delay, jitter) = (ms("BWRELAY_DELAY_MS"), ms("BWRELAY_JITTER_MS"));
        if delay <= 0.0 && jitter <= 0.0 {
            return None;
        }
        let (queue, mut pieces) = mpsc::unbounded_channel::<(Instant, Piece)>();
        tokio::spawn(async move {
            while let Some((at, (out, piece, permit))) = pieces.recv().await {
                tokio::time::sleep_until(at.into()).await;
                out.send(piece, permit);
            }
        });
        Some(DelayLine {
            queue,
            delay: Duration::from_secs_f64(delay / 1000.0),
            jitter_us: (jitter * 1000.0) as u64,
            seed: Arc::new(AtomicU64::new(0x9E37_79B9_7F4A_7C15)),
        })
    }

    fn send(&self, piece: Piece) -> bool {
        let mut x = self.seed.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.seed.store(x, Ordering::Relaxed);
        let extra = if self.jitter_us > 0 {
            x % self.jitter_us
        } else {
            0
        };
        self.queue
            .send((
                Instant::now() + self.delay + Duration::from_micros(extra),
                piece,
            ))
            .is_ok()
    }
}

/// One direction of the modeled link: a bounded FIFO queue drained at the rate. A sender that
/// fills it stops being read, the back-pressure a full buffer applies.
struct Link {
    queue: mpsc::Sender<Piece>,
}

impl Link {
    fn start(rate: Arc<AtomicU64>, depth: usize) -> Link {
        let (queue, mut pieces) = mpsc::channel::<Piece>((depth / GRAIN).max(1));
        let mut bucket = Bucket::new(rate);
        let delay = DelayLine::from_env();
        tokio::spawn(async move {
            while let Some((out, piece, permit)) = pieces.recv().await {
                bucket.take(piece.len()).await;
                match &delay {
                    Some(delay) => {
                        delay.send((out, piece, permit));
                    }
                    None => {
                        out.send(piece, permit);
                    }
                }
            }
        });
        Link { queue }
    }

    /// Move one connection's bytes onto this direction until it ends.
    async fn carry(&self, mut reader: OwnedReadHalf, out: Out) {
        let mut buf = vec![0u8; CHUNK];
        'read: while let Ok(n) = reader.read(&mut buf).await {
            if n == 0 {
                break;
            }
            for piece in buf[..n].chunks(GRAIN) {
                let Ok(permit) = Arc::clone(&out.room).acquire_owned().await else {
                    break 'read;
                };
                if self
                    .queue
                    .send((out.clone(), piece.to_vec(), permit))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        // The other side is closed as soon as this one ends.
        out.close();
    }
}

/// Copy one direction unshaped until it ends.
async fn pass(mut reader: OwnedReadHalf, out: Out, delay: Option<DelayLine>) {
    let mut buf = vec![0u8; CHUNK];
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let Ok(permit) = Arc::clone(&out.room).acquire_owned().await else {
            break;
        };
        let sent = match &delay {
            Some(delay) => delay.send((out.clone(), buf[..n].to_vec(), permit)),
            None => out.send(buf[..n].to_vec(), permit),
        };
        if !sent {
            break;
        }
    }
    out.close();
}

/// Take `down KBIT_S` and `up KBIT_S` lines from stdin until it ends, each setting that
/// direction's rate; an unshaped uplink stays unshaped.
async fn rates_from_stdin(down: Arc<AtomicU64>, up: Option<Arc<AtomicU64>>) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut words = line.split_whitespace();
        let (Some(which), Some(Ok(kbit))) = (words.next(), words.next().map(str::parse::<u64>))
        else {
            continue;
        };
        let rate = match which {
            "down" => &down,
            "up" => match &up {
                Some(up) => up,
                None => continue,
            },
            _ => continue,
        };
        rate.store(kbit * 1000 / 8, Ordering::Relaxed);
        println!("[relay] {which}link cap {kbit} kbit/s");
    }
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
    let bytes_per_s = |kbit: u64| Arc::new(AtomicU64::new(kbit * 1000 / 8));
    let (down_rate, up_rate) = (bytes_per_s(down_kbit), bytes_per_s(up_kbit));
    let down = Arc::new(Link::start(Arc::clone(&down_rate), depth));
    let up = (up_kbit > 0).then(|| Arc::new(Link::start(Arc::clone(&up_rate), depth)));
    tokio::spawn(rates_from_stdin(
        down_rate,
        (up_kbit > 0).then_some(up_rate),
    ));
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
    let up_delay = DelayLine::from_env();
    loop {
        let Ok((client, _)) = listener.accept().await else {
            continue;
        };
        let (down, up) = (Arc::clone(&down), up.clone());
        let (down_bytes, up_bytes) = (Arc::clone(&down_bytes), Arc::clone(&up_bytes));
        let up_delay = up_delay.clone();
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
            // The relay forwards what it has at once: with Nagle on, a small write behind
            // unacked data (an audio frame, a ping) waits for the peer's delayed ack and
            // reads as 20-40 ms of queue the modeled path never had.
            let _ = client.set_nodelay(true);
            let _ = server.set_nodelay(true);
            let (client_r, client_w) = client.into_split();
            let (server_r, server_w) = server.into_split();
            let (client_w, server_w) = (
                Out::start(client_w, down_bytes),
                Out::start(server_w, up_bytes),
            );
            let upward = async {
                match &up {
                    Some(link) => link.carry(client_r, server_w).await,
                    None => pass(client_r, server_w, up_delay).await,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;

    /// A connected pair over loopback, the far end's receive buffer `rcvbuf` bytes where one
    /// is named: (the end the relay holds, the far end).
    async fn pair(rcvbuf: Option<u32>) -> (TcpStream, TcpStream) {
        let socket = TcpSocket::new_v4().unwrap();
        if let Some(bytes) = rcvbuf {
            socket.set_recv_buffer_size(bytes).unwrap();
        }
        socket.bind(([127, 0, 0, 1], 0).into()).unwrap();
        let listener = socket.listen(1).unwrap();
        let addr = listener.local_addr().unwrap();
        let (near, far) = tokio::join!(TcpStream::connect(addr), listener.accept());
        (near.unwrap(), far.unwrap().0)
    }

    /// A bulk sender whose far end reads nothing fills its window and stops; a second
    /// connection's bytes cross the same shaped direction meanwhile.
    #[tokio::test(flavor = "current_thread")]
    async fn a_far_end_that_stops_reading_holds_no_other_connections_bytes() {
        let carried = Arc::new(AtomicU64::new(0));
        let link = Arc::new(Link::start(Arc::new(AtomicU64::new(750_000)), RCVBUF));
        let (mut bulk_src, bulk_in) = pair(None).await;
        let (bulk_out, _bulk_far) = pair(Some(4096)).await;
        let (mut small_src, small_in) = pair(None).await;
        let (small_out, mut small_far) = pair(None).await;
        for (reader, writer) in [(bulk_in, bulk_out), (small_in, small_out)] {
            let (reader, _) = reader.into_split();
            let (_, writer) = writer.into_split();
            let (link, carried) = (Arc::clone(&link), Arc::clone(&carried));
            tokio::spawn(async move {
                link.carry(reader, Out::start(writer, carried)).await;
            });
        }
        tokio::spawn(async move {
            let blob = vec![0u8; CHUNK];
            while bulk_src.write_all(&blob).await.is_ok() {}
        });
        // Long enough for the bulk connection to have filled its window at the hop's rate.
        tokio::time::sleep(Duration::from_secs(4)).await;
        tokio::spawn(async move {
            for _ in 0..50 {
                if small_src.write_all(&[1]).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let mut got = 0;
        let mut byte = [0u8; 64];
        let until = Instant::now() + Duration::from_millis(1500);
        while let Ok(Ok(n)) = tokio::time::timeout_at(until.into(), small_far.read(&mut byte)).await
        {
            if n == 0 {
                break;
            }
            got += n;
        }
        assert!(
            got >= 40,
            "{got} of 50 bytes crossed beside the stalled connection"
        );
    }
}
