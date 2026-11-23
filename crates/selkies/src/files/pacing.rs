/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Pacing of bulk traffic by an end-to-end gauge: the token-bucket pacer every file transfer
//! and clipboard transfer drains through, and the uplink gauge whose verdicts walk an
//! adaptive pacer's allowance up while the path stays clear and down the moment it congests.
//!
//! Nothing on this side can see where a transfer's queue stands: an upload's is in the
//! client's kernel and first hop, a download's in whatever buffer sits ahead of the client's
//! downlink (a reverse proxy, a tunnel, a bloated modem), and this host's own send queue reads
//! empty while any of those absorbs its writes. Every session connection from that client
//! crosses the same bottleneck as the transfer, in both directions, so [`UplinkGauge`] sends a
//! protocol ping every [`UplinkGauge::PING_INTERVAL`] on each session socket and times the
//! pong: the ping queues behind a download on the way out and the pong behind an upload on the
//! way back, and a round trip inflating past a per-session floor by more than
//! [`UplinkGauge::INFLATION_US`] is the verdict a delay-based scavenger backs off on (LEDBAT's
//! discipline: take what the link has spare, yield the moment anything else waits). Browsers
//! answer pings in the network process, so no page cooperation is needed.
//!
//! A download yields to its own session's stream no further than its share of the path the
//! two fill ([`UplinkGauge::share_bps`]). The stream's steer finds the path's rate by filling
//! it: it raises the stream until a queue stands, backs off for the moment that queue takes
//! to drain, and is back at what it delivered. A scavenger beside it backs off at each of
//! those queues and the stream delivers the more for it, so the next one finds the scavenger
//! lower: the stream ends with the path and the download with its floor, on a path the
//! download was the user's to fill as the stream is. So while the round trip stands no more
//! than [`BESIDE_INFLATION_US`] over its floor, the queue of a steer at work, a congested
//! verdict takes a download no lower than [`BESIDE_SHARE`] of an even split of what it and
//! the stream moved over the last [`BESIDE_WINDOW`]: the steer then meets a path that
//! delivers the stream its half and settles there. One over that share, as a slow start
//! leaves it, the same verdict takes down to it. A download under that share is raised to
//! it a step a verdict, congested or not: against a floor from before the transfer the
//! steer's own queue reads congested two verdicts in three, and a download each of them held
//! would stay where its first seconds left it. Nor does a congested verdict act before its
//! share can be read ([`UplinkGauge::share_unread`]): it is on the queue the steer stood
//! before the download moved a byte. Past that excess the path has gone late and
//! not come back, and the download yields as to anything else. The rejected
//! alternatives: reading flat out to measure the client's rate and then settling on a fixed
//! share of it filled the very queue it existed to prevent; kernel srtt and the unsent-queue
//! ioctl on the transfer's own socket time only the first hop, so a relaying middlebox blinds
//! them, and a fixed byte threshold on that queue read every chunk written faster than the hop
//! drains as congestion, pinning downloads near the rate floor.
//!
//! The end-to-end trip also spans both event loops, so a server too loaded to serve the stream
//! reads as congestion and sheds the transfer first. `INFLATION_US` sits above scheduler
//! jitter and below what a session feels, so the queue the sawtooth sustains stays
//! imperceptible. The floor is the minimum over the last [`UplinkGauge::FLOOR_BUCKETS`] minute
//! buckets (LEDBAT's base-delay history): a floor kept forever turns a route change onto a
//! longer path into permanent congestion, while one re-measured from scratch mid-transfer
//! would read the standing queue as the base and never see congestion again.
//!
//! The per-session ping clock lives in an [`UplinkClock`] the transport keeps beside each
//! session socket for the socket's life, so the separate requests of a chunked transfer's
//! slices share one floor and a mid-transfer slice never re-baselines against its own
//! standing queue. A first transfer would: its gauge's first pong returns behind what the
//! transfer has already put in the path (a megabyte of modem buffer by the time an upload's
//! handler runs), and a floor taken there hides that queue for good. So the transport pings
//! the clock on its own beat while no transfer does ([`UplinkClock::keep_floor`]), and each
//! pong is folded into the floor as it arrives: the floor a transfer is read against is the
//! path's from before it.
//!
//! An allowance is raised only while the transfers fill four fifths of it
//! (`TransferPacer::FILLED`). A clear verdict says the path carried what was sent, and of an
//! allowance several times that it says nothing: doubled on each such verdict it stood at a
//! gigabit a second before the first congested one, on a 6 Mbit/s uplink, and took half a
//! minute of cuts to mean anything again.
//!
//! An upload is read against the uplink alone where the session's socket tells that apart
//! ([`UplinkGauge::for_upload`], [`SessionPing::way_back`]): the round trip of a ping is the
//! way out's as well, and on a downlink the stream fills its steer keeps a queue standing
//! there, none of the upload's doing.
//!
//! An upload is held by its socket's window ([`upload_window`], [`UplinkGauge::windowed`]),
//! not by its reads alone: what a sender may have on its way is what its receiver has
//! acknowledged and advertises, and a kernel left to itself advertises megabytes, which a
//! paused read takes megabytes to close and a read at the path's rate never closes at all.
//!
//! Ping payloads are per-session counters; the pong echoes its ping's payload
//! (RFC 6455), so [`UplinkClock::note_pong`] matches replies to send times and stray pongs
//! fall through unmatched. A ping failure drops that socket from the gauge; a gauge with no
//! sockets left reports not alive and the caller stops pacing, since a vanished session leaves
//! nothing to protect.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::os::fd::RawFd;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// A gauged transfer moves in chunks sized to what its current rate drains in this fraction
/// of a second, so each chunk's burst onto the link stays under the gauge's own inflation
/// threshold at any rate ([`gauged_chunk_size`]).
pub const TRANSFER_CHUNK_DELAY_BUDGET: f64 = 0.0125;
/// The smallest chunk a gauged transfer moves.
pub const TRANSFER_CHUNK_MIN_BYTES: usize = 8 * 1024;
/// The largest chunk a gauged transfer moves.
pub const TRANSFER_CHUNK_MAX_BYTES: usize = 1 << 20;
/// Below this a transfer is over before a queue could matter, and gauging it would only make
/// a small file feel slow.
pub const TRANSFER_MIN_GAUGED_BYTES: u64 = 4 * 1024 * 1024;
/// The round trip's excess over its floor up to which a download keeps its share of the path
/// beside its session's stream. Under it the queue is one a steer raises to read the path by
/// and drains by itself: against a floor taken before the transfer, the round trip beside a
/// download in a hop the two fill stands 50 to 70 ms over it at the median, past 100 ms one
/// verdict in seven and under 240 ms at its peaks. Past it the path has gone late and not
/// come back, and the download yields to the stream as to anything else.
pub const BESIDE_INFLATION_US: u32 = 250_000;
/// The share of an even split a download beside its session's stream backs off to at the
/// least: under the whole of it, so that the backoff leaves the queue room to drain.
pub const BESIDE_SHARE: f64 = 0.9;
/// How far back a download reads what it and the stream beside it moved: a few of the
/// steer's ticks and of its own verdicts, and short of the seconds a backoff's hold lasts.
pub const BESIDE_WINDOW: f64 = 1.0;

/// The receive window an upload's socket advertises while the pacer allows `rate_bps`
/// (bytes a second) over a path whose least round trip is `floor_s`, its segments `mss`
/// bytes: what the allowance sends in that round trip, which a sender needs on its way to
/// keep the rate, and in [`TRANSFER_CHUNK_DELAY_BUDGET`] more, the burst a gauged transfer
/// may put on its path, and never under two segments.
///
/// A receiver holds a sender only by its window. Reads paused behind megabytes of receive
/// buffer close nothing for megabytes, and reads at the path's rate or over leave the whole
/// buffer advertised, which the sender fills the client's first hop with: a megabyte of
/// modem buffer at 6 Mbit/s is 1.4 s for everything else the client sends, whatever the
/// pacer does next. With the window at this, what the upload can stand in that hop is the
/// chunk budget's worth of its allowance, before the first verdict and whatever the
/// verdicts say.
pub fn upload_window(rate_bps: f64, floor_s: f64, mss: u32) -> u32 {
    let window = rate_bps * (floor_s.max(0.0) + TRANSFER_CHUNK_DELAY_BUDGET);
    (window.min(f64::from(i32::MAX)) as u32).max(mss.saturating_mul(2))
}

/// The receive window of an upload's socket, held to its allowance at each piece and given
/// back as it was when the upload ends ([`UplinkGauge::release_window`]): the connection
/// outlives the request. Not on drop: a request dropped with its connection has no socket
/// to give back. The listener speaks HTTP/1.1, so the request has its connection to itself,
/// and its handler runs in the connection's own task, so the descriptor is the connection's
/// for as long as the window is.
///
/// What holds the window is the receive buffer: a socket advertises no more than its buffer
/// has free, and a buffer the process sized stays that size (`SO_RCVBUF`). The clamp
/// (`TCP_WINDOW_CLAMP`) is set beside it, at each piece since the kernel moves it with the
/// buffer; by itself it holds little: the kernel stops lowering a window at four of the
/// segments the socket announces and rounds it up to the window scale's step. So the window
/// is held only where the buffer can be it. A kernel that cannot hand a sized buffer's sizing
/// back (`SO_BUF_LOCK`, Linux 5.14) would keep the connection's later requests at that
/// size, and its sockets are left alone ([`Window::of`]); and past what the kernel lets a
/// process size a buffer to (`net.core.rmem_max`), which a fast path with a long round trip
/// needs, the socket is the kernel's again, clamp and all, until the allowance fits.
struct Window {
    fd: RawFd,
    mss: u32,
    /// The clamp and the receive buffer as found.
    was: (libc::c_int, libc::c_int),
    /// The window asked for last, 0 before any.
    set: u32,
    /// Whether the receive buffer is held to the window, the kernel's sizing of it off.
    held: bool,
    /// The most the kernel has shown it sizes a buffer for, once it has refused a window.
    most: Option<libc::c_int>,
}

/// `SO_BUF_LOCK` and its receive bit (Linux 5.14): which of a socket's buffers the process
/// sized, cleared to hand the sizing back to the kernel.
pub(crate) const SO_BUF_LOCK: libc::c_int = 72;
pub(crate) const SOCK_RCVBUF_LOCK: libc::c_int = 2;
/// Where `struct tcp_info` keeps the least round trip the connection has measured, in
/// microseconds (`tcpi_min_rtt`, Linux 4.6), all ones before any.
const TCPI_MIN_RTT: usize = 148;

impl Window {
    /// The socket's window as it stands and its segment size, or `None` where the
    /// descriptor is no TCP socket or its kernel cannot hand a buffer's sizing back.
    fn of(fd: RawFd) -> Option<Window> {
        let clamp = socket_option(fd, libc::IPPROTO_TCP, libc::TCP_WINDOW_CLAMP)?;
        let mss = socket_option(fd, libc::IPPROTO_TCP, libc::TCP_MAXSEG)?;
        let buffer = socket_option(fd, libc::SOL_SOCKET, libc::SO_RCVBUF)?;
        socket_option(fd, libc::SOL_SOCKET, SO_BUF_LOCK)?;
        Some(Window {
            fd,
            mss: u32::try_from(mss).ok()?,
            was: (clamp, buffer),
            set: 0,
            held: false,
            most: None,
        })
    }

    /// The least round trip the socket's own connection has measured, in seconds: its
    /// handshake's at the least, from before the upload's first byte.
    fn least_round_trip(&self) -> Option<f64> {
        let mut info = [0u8; TCPI_MIN_RTT + 4];
        let mut len = libc::socklen_t::try_from(info.len()).ok()?;
        // SAFETY: getsockopt writes at most `len` bytes into the buffer it is given.
        let done = unsafe {
            libc::getsockopt(
                self.fd,
                libc::IPPROTO_TCP,
                libc::TCP_INFO,
                info.as_mut_ptr().cast(),
                &raw mut len,
            )
        };
        if done != 0 || usize::try_from(len).ok()? < info.len() {
            return None;
        }
        let us = u32::from_ne_bytes(info[TCPI_MIN_RTT..].try_into().ok()?);
        (us != u32::MAX).then(|| f64::from(us) / 1e6)
    }

    /// Advertise no more than `window` bytes from here on, moved where that differs from
    /// the window set by an eighth or more.
    fn hold(&mut self, window: u32) {
        let window = if self.set > 0 && window.abs_diff(self.set) < self.set / 8 {
            self.set
        } else {
            window
        };
        let wanted = libc::c_int::try_from(window).unwrap_or(libc::c_int::MAX);
        let fits = self.most.is_none_or(|most| wanted <= most);
        if fits && (window != self.set || !self.held) {
            // The kernel keeps twice what it is asked for, for its own bookkeeping, and no
            // more than its limit for a process: short of twice, the buffer cannot be the
            // window, and that limit is not asked past again.
            let kept = set_socket_option(self.fd, libc::SOL_SOCKET, libc::SO_RCVBUF, wanted)
                .then(|| socket_option(self.fd, libc::SOL_SOCKET, libc::SO_RCVBUF))
                .flatten();
            if kept.is_some_and(|kept| kept >= wanted.saturating_mul(2)) {
                self.held = true;
            } else {
                self.most = Some(kept.unwrap_or(0) / 2);
                self.loosen();
            }
        } else if !fits && self.held {
            self.loosen();
        }
        if self.held
            && socket_option(self.fd, libc::IPPROTO_TCP, libc::TCP_WINDOW_CLAMP) != Some(wanted)
        {
            set_socket_option(self.fd, libc::IPPROTO_TCP, libc::TCP_WINDOW_CLAMP, wanted);
        }
        self.set = window;
    }

    /// Hand the socket back to the kernel: the buffer's sizing, which grows it from where it
    /// stands for what flows, and the clamp as it was found.
    fn loosen(&mut self) {
        if let Some(locks) = socket_option(self.fd, libc::SOL_SOCKET, SO_BUF_LOCK) {
            set_socket_option(
                self.fd,
                libc::SOL_SOCKET,
                SO_BUF_LOCK,
                locks & !SOCK_RCVBUF_LOCK,
            );
        }
        set_socket_option(
            self.fd,
            libc::IPPROTO_TCP,
            libc::TCP_WINDOW_CLAMP,
            self.was.0,
        );
        self.held = false;
    }

    /// Give the window back as it was found: the buffer no smaller than it was, or than a
    /// process may size one (`net.core.rmem_max`, past which only the kernel's own tuning
    /// grows it back), its sizing the kernel's, the clamp its value. Whether or not the
    /// last hold took: one loosened or refused leaves the buffer where a hold before it
    /// sized it.
    fn release(&mut self) {
        if self.set == 0 {
            return;
        }
        set_socket_option(self.fd, libc::SOL_SOCKET, libc::SO_RCVBUF, self.was.1 / 2);
        self.loosen();
        self.set = 0;
    }
}

/// An int option of a socket, or `None` where the kernel refuses.
pub(crate) fn socket_option(
    fd: RawFd,
    level: libc::c_int,
    option: libc::c_int,
) -> Option<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>()).ok()?;
    // SAFETY: getsockopt writes at most `len` bytes into the int it is given.
    let done =
        unsafe { libc::getsockopt(fd, level, option, (&raw mut value).cast(), &raw mut len) };
    (done == 0).then_some(value)
}

/// Set an int option of a socket; whether the kernel took it.
fn set_socket_option(
    fd: RawFd,
    level: libc::c_int,
    option: libc::c_int,
    value: libc::c_int,
) -> bool {
    let len = libc::socklen_t::try_from(std::mem::size_of::<libc::c_int>()).unwrap_or(4);
    // SAFETY: setsockopt reads `len` bytes of the int it is given.
    unsafe { libc::setsockopt(fd, level, option, (&raw const value).cast(), len) == 0 }
}

/// Seconds on a monotonic clock whose origin is the first call: the pacers' and the gauge's
/// shared notion of now, comparable across every instance in the process.
pub fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// The chunk a gauged transfer moves next, sized to the pacer's rate.
///
/// A chunk is a burst the link absorbs at line rate whatever the average the bucket paces:
/// an upload's read size is handed back to the client as receive window and sent at once,
/// and a download's write lands in the socket whole. A fixed size would put several times
/// the gauge's delay budget onto a slow link per chunk, and the gauge would read the
/// transfer's own bursts as congestion and never let it grow; sizing to
/// [`TRANSFER_CHUNK_DELAY_BUDGET`] of the current rate keeps the burst below
/// [`UplinkGauge::INFLATION_US`] on any link.
pub fn gauged_chunk_size(pacer: &TransferPacer) -> usize {
    let sized = (pacer.rate_bps() * TRANSFER_CHUNK_DELAY_BUDGET) as usize;
    sized.clamp(TRANSFER_CHUNK_MIN_BYTES, TRANSFER_CHUNK_MAX_BYTES)
}

/// Pace `nbytes` a gauged transfer holds through `pacer`, one [`gauged_chunk_size`] piece
/// at a time, folding a fresh `gauge` verdict in before each piece.
///
/// The allowance moves only on a verdict, and verdicts come at the gauge's ping cadence, so
/// the gauge has to be sampled at that cadence however much the transfer holds at once. A
/// download reads one piece at a time, but an upload's body arrives in whatever the HTTP
/// stack read (hundreds of KiB on a fast link), and pacing such a chunk on one verdict
/// would grow the allowance once per chunk rather than once per ping: the slow start that
/// should double every round trip doubles every second or two. The pieces are sized again
/// from the rate each verdict leaves.
///
/// Returns `false` once the gauge has no session socket left, with the rest of `nbytes`
/// unpaced: the session it protected is gone, and the caller stops gauging.
pub async fn pace_gauged(pacer: &TransferPacer, gauge: &mut UplinkGauge, nbytes: usize) -> bool {
    let mut rest = nbytes;
    while rest > 0 {
        let verdict = gauge.sample();
        if !gauge.alive() {
            gauge.release_window();
            return false;
        }
        gauge.follow(pacer.rate_bps());
        let piece = rest.min(gauged_chunk_size(pacer));
        let share = gauge.share_bps(piece);
        // Until its share can be read a download beside its session's stream has moved next
        // to nothing, and a congested verdict is on the queue the stream's steer stood
        // before it. A clear one says the path has room, and acts as on any path.
        let verdict = verdict.filter(|congested| !congested || !gauge.share_unread());
        pacer
            .pace_verdict_beside(piece, verdict, Some(gauge.inflation_us()), share)
            .await;
        rest -= piece;
    }
    true
}

/// A session socket the gauge can ping: the transport implements it over the socket's writer.
pub trait SessionPing: Send + Sync {
    /// Send a WebSocket ping carrying `payload` behind everything written to the socket so
    /// far; `false` when the socket is gone, which drops it from every gauge.
    fn ping(&self, payload: &[u8]) -> bool;

    /// The bytes of the session's stream written to the socket so far, where the transport
    /// counts them: what a download beside the stream reads the stream's rate by
    /// ([`UplinkGauge::share_bps`]).
    fn stream_bytes(&self) -> Option<u64> {
        None
    }

    /// How long the socket's way back holds what its page sends, past the least it took, in
    /// seconds, where the transport reads that off the page's own stamps: the client's
    /// uplink alone, which is what an upload shares with the session
    /// ([`UplinkGauge::for_upload`]). `None` where nothing tells it at present.
    fn way_back(&self) -> Option<f64> {
        None
    }
}

/// The RTT floor of one session: the minimum over a bounded history of minute buckets.
#[derive(Debug, Default)]
pub struct FloorHistory {
    buckets: VecDeque<(i64, u32)>,
}

impl FloorHistory {
    /// An empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one RTT sample taken at `now` (seconds) into the history and return the floor.
    pub fn observe(&mut self, rtt_us: u32, now: f64) -> u32 {
        let bucket = (now / UplinkGauge::FLOOR_BUCKET_SECONDS).floor() as i64;
        match self.buckets.back_mut() {
            Some((last, floor)) if *last == bucket => {
                if rtt_us < *floor {
                    *floor = rtt_us;
                }
            }
            _ => {
                if self.buckets.len() == UplinkGauge::FLOOR_BUCKETS {
                    self.buckets.pop_front();
                }
                self.buckets.push_back((bucket, rtt_us));
            }
        }
        self.buckets.iter().map(|(_, f)| *f).min().unwrap_or(rtt_us)
    }
}

/// The ping clock and floor history of one session socket.
struct ClockState {
    /// Payloads in flight with their send times, oldest first.
    pending: VecDeque<([u8; 8], f64)>,
    rtt_us: Option<u32>,
    /// Count of answered pings, which a gauge compares to tell a fresh pong.
    seq: u64,
    next_ping: u64,
    floor: FloorHistory,
    /// The floor as the newest answered ping left it.
    floor_us: Option<u32>,
    /// When the last ping went out.
    pinged_at: f64,
    /// When the newest answered ping went out: everything written before it reached the peer.
    answered: Option<f64>,
}

/// The uplink ping clock of one session socket, kept by its transport for the socket's life.
///
/// Successive transfers from the same client share its floor history through it, and the
/// transport resolves every PONG it receives against it ([`UplinkClock::note_pong`]); the
/// sockets run with the stack's automatic ping off so the frames reach the transport.
pub struct UplinkClock {
    pinger: Arc<dyn SessionPing>,
    state: Mutex<ClockState>,
}

impl UplinkClock {
    /// A clock pinging through `pinger`.
    pub fn new(pinger: Arc<dyn SessionPing>) -> Self {
        UplinkClock {
            pinger,
            state: Mutex::new(ClockState {
                pending: VecDeque::new(),
                rtt_us: None,
                seq: 0,
                next_ping: 0,
                floor: FloorHistory::new(),
                floor_us: None,
                pinged_at: f64::NEG_INFINITY,
                answered: None,
            }),
        }
    }

    /// Ping behind everything written to the socket so far; `false` when the socket is gone.
    pub fn ping_behind(&self) -> bool {
        let payload = {
            let mut state = self.state.lock();
            let payload = state.next_ping.to_be_bytes();
            state.next_ping += 1;
            let now = monotonic();
            state.pinged_at = now;
            state.pending.push_back((payload, now));
            while state.pending.len() > UplinkGauge::PENDING_MAX {
                state.pending.pop_front();
            }
            payload
        };
        self.pinger.ping(&payload)
    }

    /// Resolve a received PONG payload against the clock; a payload the clock did not send
    /// (a peer's unsolicited pong) misses the pending set and is ignored.
    pub fn note_pong(&self, data: &[u8]) {
        let mut state = self.state.lock();
        let Some(index) = state.pending.iter().position(|(p, _)| p == data) else {
            return;
        };
        let Some((_, sent)) = state.pending.remove(index) else {
            return;
        };
        let now = monotonic();
        let rtt = ((now - sent) * 1e6).max(0.0) as u32;
        state.rtt_us = Some(rtt);
        state.floor_us = Some(state.floor.observe(rtt, now));
        state.seq += 1;
        state.answered = Some(state.answered.map_or(sent, |a| a.max(sent)));
    }

    /// The round trip last measured, in milliseconds rounded to a tenth, for `/api/sessions`.
    pub fn rtt_ms(&self) -> Option<f64> {
        let rtt = self.state.lock().rtt_us?;
        if rtt == 0 {
            return None;
        }
        Some((f64::from(rtt) / 100.0).round() / 10.0)
    }

    /// When the newest answered ping went out, on the [`monotonic`] clock: everything
    /// written to the socket before it has reached the peer.
    pub fn answered(&self) -> Option<f64> {
        self.state.lock().answered
    }

    /// The bytes of the session's stream written to the socket so far
    /// ([`SessionPing::stream_bytes`]).
    pub fn stream_bytes(&self) -> Option<u64> {
        self.pinger.stream_bytes()
    }

    fn seq(&self) -> u64 {
        self.state.lock().seq
    }

    /// The fresh RTT past `last_seq` with the floor it was folded into as it arrived:
    /// `(rtt_us, floor_us, seq)`.
    fn fresh(&self, last_seq: u64) -> Option<(u32, u32, u64)> {
        let state = self.state.lock();
        if state.seq == last_seq {
            return None;
        }
        let rtt = state.rtt_us?;
        Some((rtt, state.floor_us.unwrap_or(rtt), state.seq))
    }

    /// Keep the floor the path's own while no transfer pings: one ping once none has gone
    /// out for [`UplinkGauge::FLOOR_PING_INTERVAL`], for the socket's transport to call on
    /// its own beat. A gauge that starts pinging with a transfer finds a floor taken before
    /// the transfer's queue stood, where its own first pong returns behind that queue.
    /// `false` when the socket is gone.
    pub fn keep_floor(&self) -> bool {
        let due = monotonic() - self.state.lock().pinged_at >= UplinkGauge::FLOOR_PING_INTERVAL;
        !due || self.ping_behind()
    }

    /// How long the socket's way back holds what its page sends ([`SessionPing::way_back`]).
    pub fn way_back(&self) -> Option<f64> {
        self.pinger.way_back()
    }

    /// The least round trip the socket's history holds, in seconds, once a ping was answered.
    pub fn floor_s(&self) -> Option<f64> {
        self.state.lock().floor_us.map(|us| f64::from(us) / 1e6)
    }

    /// Test hook: no ping has gone out for `seconds` more.
    #[cfg(test)]
    fn idle_for(&self, seconds: f64) {
        self.state.lock().pinged_at -= seconds;
    }

    #[cfg(test)]
    fn backdate_pending(&self, payload: &[u8], by_seconds: f64) {
        let mut state = self.state.lock();
        if let Some(entry) = state.pending.iter_mut().find(|(p, _)| p == payload) {
            entry.1 -= by_seconds;
        }
    }
}

/// One session connection a transfer's uplink gauge may ping, as the transport lists them.
#[derive(Clone)]
pub struct UplinkSession {
    /// The socket's ping clock.
    pub clock: Arc<UplinkClock>,
    /// The session token the socket authenticated with, where it did.
    pub token: Option<String>,
    /// The peer address the socket came from.
    pub remote: Option<IpAddr>,
}

/// One session socket a gauge reads.
struct Gauged {
    clock: Arc<UplinkClock>,
    /// The clock's count of answered pings as the gauge last read it.
    seq: u64,
    /// The socket's stream bytes as a download's gauge last read them.
    streamed: Option<u64>,
}

/// Congestion verdicts for one client's bulk transfer, timed end to end over that client's
/// own session socket(s) (module docs).
pub struct UplinkGauge {
    conns: Vec<Gauged>,
    last_ping: f64,
    inflation_us: u32,
    /// Where the gauge is a download's ([`UplinkGauge::beside_stream`]): when, with what the
    /// download had moved by then and what the gauged sockets' streams had written.
    beside: Option<VecDeque<(f64, u64, u64)>>,
    /// What the gauged sockets' streams have written while gauged, each socket's counted
    /// from its first reading: a socket that leaves the gauge takes nothing it wrote out.
    streamed: u64,
    /// The bytes the transfer has moved under this gauge.
    moved: u64,
    /// Whether a download's share is yet to be read, its split having been counted for
    /// less than half of [`BESIDE_WINDOW`] ([`UplinkGauge::share_unread`]).
    unread: bool,
    /// Whether the gauge is an upload's ([`UplinkGauge::for_upload`]).
    upload: bool,
    /// Where the gauge is an upload's on a TCP socket ([`UplinkGauge::windowed`]): that
    /// socket's window.
    window: Option<Window>,
}

impl UplinkGauge {
    /// Seconds between pings on each gauged socket.
    pub const PING_INTERVAL: f64 = 0.2;
    /// Seconds between the pings that keep a socket's floor while no transfer is gauged
    /// ([`UplinkClock::keep_floor`]): several to each of the floor's buckets.
    pub const FLOOR_PING_INTERVAL: f64 = 10.0;
    /// The excess over the floor that reads as congestion.
    pub const INFLATION_US: u32 = 25_000;
    /// The width of one floor bucket.
    pub const FLOOR_BUCKET_SECONDS: f64 = 60.0;
    /// How many buckets the floor remembers.
    pub const FLOOR_BUCKETS: usize = 10;
    /// Unanswered pings kept per socket before the oldest is forgotten.
    pub const PENDING_MAX: usize = 25;

    /// A gauge over the sockets `clocks`, each read from where its clock stands now.
    pub fn new(clocks: impl IntoIterator<Item = Arc<UplinkClock>>) -> Self {
        UplinkGauge {
            conns: clocks
                .into_iter()
                .map(|clock| Gauged {
                    seq: clock.seq(),
                    clock,
                    streamed: None,
                })
                .collect(),
            last_ping: f64::NEG_INFINITY,
            inflation_us: 0,
            beside: None,
            streamed: 0,
            moved: 0,
            unread: false,
            upload: false,
            window: None,
        }
    }

    /// The gauge of an upload: its bytes cross the client's uplink and nothing else of the
    /// session's path, so it reads the uplink alone where a gauged socket tells it
    /// ([`SessionPing::way_back`]). A ping's round trip carries the way out too, where the
    /// stream's own steer keeps a queue standing on a downlink it fills, and an upload read
    /// against that queue backs off from an uplink it has to itself. What the way back
    /// holds is read off the page's clock against this one, and carries how far the two
    /// have drifted apart since its least was taken. Each reading is the uplink's queue and
    /// something more, so the queue is no more than the lesser of the two, and that is what
    /// the verdict is on.
    #[must_use]
    pub fn for_upload(mut self) -> Self {
        self.upload = true;
        self
    }

    /// The gauge of an upload arriving on the TCP socket `fd`, valid for as long as the
    /// gauge: the socket's window follows the allowance ([`UplinkGauge::follow`]). Without
    /// a descriptor, or on one that is no TCP socket, the gauge paces by its reads alone.
    #[must_use]
    pub fn windowed(mut self, fd: Option<RawFd>) -> Self {
        self.window = fd.and_then(Window::of);
        self
    }

    /// Hold the upload's socket to the window its allowance of `rate_bps` warrants
    /// ([`upload_window`]) over the gauged sockets' least round trip, or over the least its
    /// own connection has measured where that is longer: the sockets gauged are the
    /// session's, and a window under what the upload's own path takes to turn around would
    /// hold it under its allowance, which is then never raised.
    pub fn follow(&mut self, rate_bps: f64) {
        let Some(window) = self.window.as_mut() else {
            return;
        };
        let round_trip = self
            .conns
            .iter()
            .filter_map(|gauged| gauged.clock.floor_s())
            .reduce(f64::min)
            .into_iter()
            .chain(window.least_round_trip())
            .reduce(f64::max)
            .unwrap_or(0.0);
        window.hold(upload_window(rate_bps, round_trip, window.mss));
    }

    /// Give the upload's socket its window back as it was found, the upload over or no
    /// longer gauged.
    pub fn release_window(&mut self) {
        if let Some(mut window) = self.window.take() {
            window.release();
        }
    }

    /// The gauge of a download: its bytes cross the path the gauged sockets' streams cross,
    /// the same way, and it keeps its share of that path beside them
    /// ([`UplinkGauge::share_bps`]). An upload's bytes go the other way, where the stream
    /// sends its acks alone ([`UplinkGauge::for_upload`]).
    #[must_use]
    pub fn beside_stream(mut self) -> Self {
        self.beside = Some(VecDeque::new());
        self
    }

    /// Count the `nbytes` a download moves next, and return the least a congested verdict
    /// backs it off to, in bytes a second: [`BESIDE_SHARE`] of an even split of what it and
    /// the gauged sockets' streams moved over the last [`BESIDE_WINDOW`]. `None` where the
    /// gauge is not a download's, where no gauged socket counts a stream, and until half
    /// that window has been read.
    pub fn share_bps(&mut self, nbytes: usize) -> Option<f64> {
        self.share_bps_at(nbytes, monotonic())
    }

    /// [`UplinkGauge::share_bps`] at `now` on the [`monotonic`] clock.
    fn share_bps_at(&mut self, nbytes: usize, now: f64) -> Option<f64> {
        let mut counted = false;
        for gauged in &mut self.conns {
            let Some(written) = gauged.clock.stream_bytes() else {
                continue;
            };
            counted = true;
            let since = written.saturating_sub(gauged.streamed.unwrap_or(written));
            self.streamed += since;
            gauged.streamed = Some(written);
        }
        let stream = counted.then_some(self.streamed);
        // What has moved by now: the piece counted here moves after it.
        let so_far = self.moved;
        self.moved += nbytes as u64;
        self.unread = false;
        let history = self.beside.as_mut()?;
        let Some(stream) = stream else {
            history.clear();
            return None;
        };
        if history
            .back()
            .is_none_or(|(at, _, _)| now - at >= BESIDE_WINDOW / 10.0)
        {
            history.push_back((now, so_far, stream));
        }
        while history
            .get(1)
            .is_some_and(|(at, _, _)| now - at >= BESIDE_WINDOW)
        {
            history.pop_front();
        }
        let (at, moved, streamed) = *history.front()?;
        let span = now - at;
        if span < BESIDE_WINDOW / 2.0 {
            self.unread = true;
            return None;
        }
        let both = (so_far - moved) + stream.saturating_sub(streamed);
        Some(BESIDE_SHARE * both as f64 / span / 2.0)
    }

    /// Whether the last [`UplinkGauge::share_bps`] had a stream to split the path with and
    /// too little of the split counted to say the share: a congested verdict at such a
    /// moment is on the path before the download, and the caller does not act on it.
    pub fn share_unread(&self) -> bool {
        self.unread
    }

    /// Whether any session socket is left to gauge.
    pub fn alive(&self) -> bool {
        !self.conns.is_empty()
    }

    /// The largest fresh excess over the floor the last verdict saw, the queue's depth in
    /// time, for the caller to size its step by.
    pub fn inflation_us(&self) -> u32 {
        self.inflation_us
    }

    /// Ping on cadence and return one verdict across the gauged sockets.
    ///
    /// `Some(true)` when any session's round trip stands above its floor by more than
    /// [`UplinkGauge::INFLATION_US`], `Some(false)` when at least one session answered fresh
    /// and none congested, `None` when no fresh pong arrived since the last call (the caller
    /// holds its rate). An upload's gauge takes what a socket's way back holds for that
    /// excess, where the socket tells it and it is the lesser
    /// ([`UplinkGauge::for_upload`]).
    pub fn sample(&mut self) -> Option<bool> {
        let now = monotonic();
        if now - self.last_ping >= Self::PING_INTERVAL {
            self.last_ping = now;
            self.conns.retain(|gauged| gauged.clock.ping_behind());
        }
        let mut verdict: Option<bool> = None;
        let mut inflation = 0u32;
        for gauged in &mut self.conns {
            let Some((rtt, floor, seq)) = gauged.clock.fresh(gauged.seq) else {
                continue;
            };
            gauged.seq = seq;
            let held = if self.upload {
                gauged.clock.way_back()
            } else {
                None
            };
            let over = held.map_or(rtt.saturating_sub(floor), |held| {
                let held = (held.max(0.0) * 1e6).min(f64::from(u32::MAX)) as u32;
                held.min(rtt.saturating_sub(floor))
            });
            inflation = inflation.max(over);
            verdict = Some(verdict.unwrap_or(false) || over > Self::INFLATION_US);
        }
        if verdict.is_some() {
            self.inflation_us = inflation;
        }
        verdict
    }
}

/// An [`UplinkGauge`] over one session socket alone.
///
/// For a sender pacing what it writes to that socket: the pong queues behind everything
/// already written to it, so its round trip is what the bytes in front of it cost. The
/// uploads' gauge instead spans every session socket of one client, because an upload's
/// congestion is on a client uplink those sockets share rather than on any one of them.
pub fn socket_gauge(clock: Arc<UplinkClock>) -> UplinkGauge {
    UplinkGauge::new([clock])
}

/// The allowance's bucket and congestion state.
struct PacerState {
    rate_bps: f64,
    tokens: f64,
    ts: f64,
    congested: bool,
    probe_ceiling: Option<f64>,
    slow_start: bool,
    hold_until: f64,
    last_inflation_us: Option<u32>,
    /// Whether the hold in force has brought the allowance down to what the transfers moved.
    aligned: bool,
    /// What the transfers have drained in all, when and how much of it they had within the
    /// last [`TransferPacer::USED_SECONDS`] (oldest first), and the rate that shows.
    drained: f64,
    drains: VecDeque<(f64, f64)>,
    used_bps: Option<f64>,
}

/// Rate pacing for bulk traffic, one instance per allowance.
///
/// Two modes. A static cap (`static_bps`) exists for links whose rate the operator already
/// knows: one such pacer is shared by every file transfer on the server, so the link sees one
/// source regardless of how many sockets a browser opens. Without one, an adaptive pacer holds
/// a transfer inside an allowance that walks up while the path stays clear and down the moment
/// it congests, fed [`UplinkGauge`] verdicts through [`TransferPacer::pace_verdict`]: the file
/// downloads share one, the file uploads another (their queues stand at opposite ends of the
/// path), and a clipboard transfer paces its own against the socket it is writing to. No link
/// estimate is needed, and a transfer with no session socket to gauge rides the static cap
/// alone rather than being throttled blindly.
pub struct TransferPacer {
    static_bps: u64,
    adaptive: bool,
    inner: Mutex<PacerState>,
}

impl TransferPacer {
    /// The chunk the pacer's own tests move.
    pub const CHUNK: usize = 256 * 1024;
    /// The rate no cut goes under.
    pub const RATE_FLOOR: f64 = 48.0 * 1024.0;
    /// The seconds a cut holds growth and further cuts while the queue it cut for drains.
    const HOLD_SECONDS: f64 = 1.5;
    /// The seconds back the transfers' own rate is read over, and the share of the allowance
    /// they must have moved in them for a clear verdict to raise it: an allowance the
    /// transfers leave unfilled was not what the path carried, and a clear verdict says
    /// nothing of it. The seconds are a verdict's and a quarter more, read up to the verdict
    /// itself: an allowance just doubled is found nine tenths filled by the next verdict
    /// where the sender keeps up, so the ramp toward an unknown link is as steep as it was,
    /// and one the path already holds under is not doubled again. An allowance over the
    /// path's rate is a queue in the path, the longer the further over: four fifths keeps
    /// it within a quarter of that rate.
    const USED_SECONDS: f64 = 0.25;
    const FILLED: f64 = 0.8;
    /// The idle seconds after which the allowance is stale and the multiplicative ramp is
    /// re-entered (TCP's restart after idle), and the transfers' rate is read afresh.
    const RESTART_SECONDS: f64 = 10.0;

    /// A pacer: `static_bps` caps it (0 for none) and `adaptive` lets verdicts steer it.
    pub fn new(static_bps: u64, adaptive: bool) -> Self {
        let rate = if static_bps > 0 {
            static_bps as f64
        } else {
            256.0 * 1024.0
        };
        TransferPacer {
            static_bps,
            adaptive,
            inner: Mutex::new(PacerState {
                rate_bps: rate,
                tokens: rate * 0.5,
                ts: monotonic(),
                congested: false,
                probe_ceiling: None,
                slow_start: true,
                hold_until: 0.0,
                last_inflation_us: None,
                aligned: false,
                drained: 0.0,
                drains: VecDeque::new(),
                used_bps: None,
            }),
        }
    }

    /// The operator's cap, shared by every transfer; 0 paces nothing.
    pub fn static_cap(static_bps: u64) -> Self {
        Self::new(static_bps, false)
    }

    /// A purely adaptive pacer, steered by gauge verdicts.
    pub fn adaptive() -> Self {
        Self::new(0, true)
    }

    /// Whether the pacer paces at all: adaptive, or capped.
    pub fn active(&self) -> bool {
        self.adaptive || self.static_bps > 0
    }

    /// The operator's cap in bytes per second, 0 for none.
    pub fn static_bps(&self) -> u64 {
        self.static_bps
    }

    /// The allowance in force, in bytes per second.
    pub fn rate_bps(&self) -> f64 {
        self.inner.lock().rate_bps
    }

    /// The static cap, or effectively unbounded when purely adaptive.
    fn ceiling(&self) -> f64 {
        if self.static_bps > 0 {
            self.static_bps as f64
        } else {
            64.0 * 1024.0 * 1024.0 * 1024.0
        }
    }

    /// Sleep off what `nbytes` overdraws from the allowance as it stands.
    ///
    /// The static leg: no verdict is folded in, so on a purely adaptive pacer this holds the
    /// rate the verdicts last set. After a long idle gap the remembered rate is stale, so the
    /// multiplicative ramp is re-entered (TCP's restart after idle): a link that got faster
    /// meanwhile is rediscovered in chunks, not minutes, and one that got slower is cut by
    /// the first gauge sample. The deficit is slept off here and paid back by the next call's
    /// elapsed-time refill; zeroing the balance after the sleep would credit the slept
    /// interval twice and double the delivered rate.
    pub async fn pace(&self, nbytes: usize) {
        if !self.active() {
            return;
        }
        if let Some(wait) = self.bucket(nbytes) {
            tokio::time::sleep(wait).await;
        }
    }

    /// Adaptive leg: fold one [`UplinkGauge`] verdict in, then drain.
    ///
    /// `None` (no fresh sample) holds the rate and still drains the bucket; `Some` is one
    /// `gauge_backoff` step, with the gauge's measured inflation telling a draining queue
    /// from a standing one. The cut is gentle, since a delay verdict fires at a bounded queue
    /// where a loss-like signal would mean one already overflowed, and the growth step is
    /// proportional rather than a fixed 8 KiB: verdicts arrive at the gauge's ping cadence, a
    /// few per second, and a fixed step at that cadence would take minutes to recover a fast
    /// link's post-cut rate. Together they hold the AIMD sawtooth's duty cycle near the line
    /// instead of near half of it.
    pub async fn pace_verdict(
        &self,
        nbytes: usize,
        congested: Option<bool>,
        inflation_us: Option<u32>,
    ) {
        self.pace_verdict_beside(nbytes, congested, inflation_us, None)
            .await;
    }

    /// [`TransferPacer::pace_verdict`] for a download beside its session's stream: a
    /// congested verdict backs the allowance off no further than `share_bps`
    /// ([`UplinkGauge::share_bps`]) while the round trip stands no more than
    /// [`BESIDE_INFLATION_US`] over its floor.
    pub async fn pace_verdict_beside(
        &self,
        nbytes: usize,
        congested: Option<bool>,
        inflation_us: Option<u32>,
        share_bps: Option<f64>,
    ) {
        if !self.active() {
            return;
        }
        if self.adaptive
            && let Some(congested) = congested
        {
            let step = (8.0 * 1024.0f64).max((self.rate_bps() * 0.03).floor());
            let share =
                share_bps.filter(|_| inflation_us.is_some_and(|i| i <= BESIDE_INFLATION_US));
            self.gauge_backoff_beside(congested, !congested, 0.65, step, inflation_us, share);
        }
        if let Some(wait) = self.bucket(nbytes) {
            tokio::time::sleep(wait).await;
        }
    }

    /// Drain `nbytes` from the token bucket; the overdraw to sleep off, if any.
    fn bucket(&self, nbytes: usize) -> Option<Duration> {
        let now = monotonic();
        let mut s = self.inner.lock();
        // What the transfers moved up to now, the piece drained here moving after it. A pause
        // within a transfer is time it moved nothing in, read into the rate like any other;
        // one as long as the restart's is a new transfer, whose reading starts over.
        if self.adaptive && now - s.ts > Self::RESTART_SECONDS {
            s.slow_start = true;
            s.drains.clear();
            s.used_bps = None;
        }
        if s.drains
            .back()
            .is_none_or(|(at, _)| now - at >= Self::USED_SECONDS / 10.0)
        {
            let so_far = s.drained;
            s.drains.push_back((now, so_far));
        }
        while s
            .drains
            .get(1)
            .is_some_and(|(at, _)| now - at >= Self::USED_SECONDS)
        {
            s.drains.pop_front();
        }
        if let Some(&(at, before)) = s.drains.front()
            && now - at >= Self::USED_SECONDS / 2.0
        {
            s.used_bps = Some((s.drained - before) / (now - at));
        }
        s.drained += nbytes as f64;
        let limit = s.rate_bps.min(self.ceiling());
        s.tokens = (limit * 0.5).min(s.tokens + (now - s.ts) * limit);
        s.ts = now;
        s.tokens -= nbytes as f64;
        if s.tokens < 0.0 {
            Some(Duration::from_secs_f64(-s.tokens / limit))
        } else {
            None
        }
    }

    /// One congestion-control step on the shared allowance: a congested sample multiplies
    /// the rate down; a clear one probes upward, multiplicatively while no congestion has
    /// ever been seen (the initial ramp toward an unknown link rate), additively by `step`
    /// after (fine-grained probing near the working point, TCP's post-ssthresh split; the
    /// caller sizes the step to its sample cadence).
    ///
    /// The recovery ceiling arms once per congestion epoch, from the rate at the epoch's
    /// first congested sample (ssthresh semantics): arming it per chunk lets a sustained
    /// spike ratchet the ceiling toward the floor, and computing it from the post-backoff
    /// rate pins recovery below the rate itself. Reaching the ceiling releases it so clear
    /// stretches keep probing past the last congested rate; that sawtooth is what keeps a
    /// link that gets faster later reachable.
    ///
    /// A cut also pauses growth and further cuts for a drain window: a sample inside it,
    /// clear or congested, still reflects the queue that cut is draining. Resuming on a clear
    /// would keep the bottleneck queue standing, and the cut never relieves the stream
    /// sharing the link; cutting again on each congested sample would take a fat buffer's
    /// worth of samples to the floor before it empties, and the recovery from there is
    /// additive. Past the window a queue still standing draws another full cut, but one the
    /// gauge measures as shrinking is already draining at the rate in force and gets a
    /// quarter of it: the full cut would only trade the drain's last seconds for a recovery
    /// that takes many times longer. The epoch closes only on a clear sample past that
    /// window: ending it on one inside would let an oscillating gauge re-arm the ceiling from
    /// each freshly cut rate, the same ratchet one flap at a time.
    ///
    /// An allowance its transfers left a fifth or more of unfilled was over what the path
    /// carried, and a cut takes it down to what they moved where that is the lower, no
    /// further than a second cut would; the hold after brings it down to that once more. A
    /// slow start ends a doubling or two past the path's rate, and that excess stands in the
    /// path for as long as the allowance keeps it: on a round trip of 80 ms the cuts alone
    /// took six seconds to bring it under 6 Mbit/s from 16.8, with 110 ms of queue standing
    /// meanwhile.
    ///
    /// `share_bps` is what a congested verdict takes the allowance to, where there is one
    /// ([`UplinkGauge::share_bps`]): toward it a step a verdict from under it, and down to
    /// it from over it.
    fn gauge_backoff_beside(
        &self,
        congested: bool,
        clear: bool,
        cut: f64,
        step: f64,
        inflation_us: Option<u32>,
        share_bps: Option<f64>,
    ) {
        let mut s = self.inner.lock();
        if congested {
            s.slow_start = false;
            let now = monotonic();
            let draining =
                matches!((inflation_us, s.last_inflation_us), (Some(i), Some(l)) if i < l);
            s.last_inflation_us = inflation_us;
            // Under its share beside its session's stream, the queue is the steer's, which
            // keeps one standing as it works: the verdict on it holds the download nowhere,
            // and it takes its share a step at a time, as on a clear path.
            if let Some(share) = share_bps.filter(|share| s.rate_bps < *share) {
                if s.used_bps
                    .is_none_or(|used| used >= Self::FILLED * s.rate_bps)
                {
                    s.rate_bps = (s.rate_bps + step).min(share).min(self.ceiling());
                }
                return;
            }
            // Over its share, the same verdict takes it down to the share: what a slow start
            // or a clear stretch added over it is the stream's again once the steer's queue
            // stands. The share is the split the two are measured at, not a cut for a queue,
            // and is followed down through a hold too.
            if let Some(share) = share_bps.filter(|share| s.rate_bps > *share) {
                if !s.congested {
                    s.congested = true;
                    s.probe_ceiling = Some(s.rate_bps.max(2.0 * Self::RATE_FLOOR));
                }
                s.rate_bps = share.max(Self::RATE_FLOOR);
                if now >= s.hold_until {
                    s.hold_until = now + Self::HOLD_SECONDS;
                    s.aligned = false;
                }
                return;
            }
            // What the transfers moved, where they left a fifth of the allowance or more
            // unfilled: the path did not carry the allowance, and the excess stands in it
            // as a queue until the allowance comes down.
            let moved = s.used_bps.filter(|used| *used < Self::FILLED * s.rate_bps);
            let least = share_bps.map_or(Self::RATE_FLOOR, |share| share.min(s.rate_bps));
            let mut eased = cut;
            if !s.congested {
                s.congested = true;
                s.probe_ceiling = Some(s.rate_bps.max(2.0 * Self::RATE_FLOOR));
            } else if now < s.hold_until {
                if let Some(moved) = moved.filter(|_| !s.aligned) {
                    s.rate_bps = moved.max(s.rate_bps * cut).max(Self::RATE_FLOOR).max(least);
                    s.aligned = true;
                }
                return;
            } else if draining {
                eased = 1.0 - (1.0 - cut) / 4.0;
            }
            let to = s.rate_bps * eased;
            let to = moved.map_or(to, |moved| to.min(moved.max(to * cut)));
            s.rate_bps = to.max(Self::RATE_FLOOR).max(least);
            s.hold_until = now + Self::HOLD_SECONDS;
            s.aligned = false;
            return;
        }
        s.last_inflation_us = None;
        if !clear || monotonic() < s.hold_until {
            return;
        }
        s.congested = false;
        let mut ceiling = s.probe_ceiling;
        if matches!(ceiling, Some(c) if s.rate_bps >= c) {
            s.probe_ceiling = None;
            ceiling = None;
        }
        let bound = ceiling.unwrap_or(s.rate_bps * 4.0).min(self.ceiling());
        if s.used_bps
            .is_some_and(|used| used < Self::FILLED * s.rate_bps)
        {
            return;
        }
        s.rate_bps = if s.slow_start {
            (s.rate_bps * 2.0).min(bound)
        } else {
            (s.rate_bps + step).min(bound)
        };
    }

    /// [`TransferPacer::gauge_backoff_beside`] with no share to keep.
    #[cfg(test)]
    fn gauge_backoff(
        &self,
        congested: bool,
        clear: bool,
        cut: f64,
        step: f64,
        inflation_us: Option<u32>,
    ) {
        self.gauge_backoff_beside(congested, clear, cut, step, inflation_us, None);
    }
}

#[cfg(test)]
mod tests {
    //! The bucket delivers the configured rate, the congestion step arms its ceiling once per epoch
    //! and probes multiplicatively only before the first congestion, an unset cap does not
    //! throttle, chunks follow the rate, and the gauge's verdicts come from timing its own pings.

    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    const DEFAULT_STEP: f64 = 8.0 * 1024.0;

    async fn timed_pace(pacer: &TransferPacer, total: usize, verdict_leg: bool) -> f64 {
        let start = Instant::now();
        let mut sent = 0;
        while sent < total {
            let n = TransferPacer::CHUNK.min(total - sent);
            if verdict_leg {
                pacer.pace_verdict(n, None, None).await;
            } else {
                pacer.pace(n).await;
            }
            sent += n;
        }
        start.elapsed().as_secs_f64()
    }

    fn set_rate(pacer: &TransferPacer, rate: f64) {
        pacer.inner.lock().rate_bps = rate;
    }

    fn release_hold(pacer: &TransferPacer) {
        pacer.inner.lock().hold_until = 0.0;
    }

    fn probe_ceiling(pacer: &TransferPacer) -> Option<f64> {
        pacer.inner.lock().probe_ceiling
    }

    #[tokio::test]
    async fn static_cap_delivers_the_configured_rate() {
        let rate = 2 * 1024 * 1024;
        let pacer = TransferPacer::static_cap(rate as u64);
        let sent = 3 * 1024 * 1024;
        let expected = (sent as f64 - f64::from(rate) * 0.5) / f64::from(rate);
        let elapsed = timed_pace(&pacer, sent, false).await;
        assert!(
            expected * 0.85 <= elapsed && elapsed <= expected * 1.4,
            "elapsed={elapsed:.2}s expected~{expected:.2}s"
        );
    }

    #[test]
    fn congestion_epochs_and_recovery() {
        let pacer = TransferPacer::adaptive();
        let r0 = pacer.rate_bps();
        for _ in 0..6 {
            pacer.gauge_backoff(false, true, 0.6, DEFAULT_STEP, None);
        }
        assert!(
            pacer.rate_bps() >= r0 * 8.0,
            "pre-congestion ramp is multiplicative"
        );

        let r_at_cong = 1024.0 * 1024.0;
        set_rate(&pacer, r_at_cong);
        pacer.gauge_backoff(true, false, 0.6, DEFAULT_STEP, None);
        let armed = probe_ceiling(&pacer).expect("ceiling armed");
        let cut_rate = pacer.rate_bps();
        pacer.gauge_backoff(true, false, 0.6, DEFAULT_STEP, None);
        assert_eq!(armed, r_at_cong.max(2.0 * TransferPacer::RATE_FLOOR));
        assert_eq!(
            probe_ceiling(&pacer),
            Some(armed),
            "ceiling arms once per epoch"
        );
        assert_eq!(
            pacer.rate_bps(),
            cut_rate,
            "a congested sample inside the drain window does not cut again"
        );
        release_hold(&pacer);
        pacer.gauge_backoff(true, false, 0.6, DEFAULT_STEP, None);
        assert!(
            pacer.rate_bps() < cut_rate,
            "a congested sample past the window cuts again"
        );
        assert_eq!(probe_ceiling(&pacer), Some(armed), "without re-arming");

        let held = pacer.rate_bps();
        pacer.gauge_backoff(false, true, 0.6, DEFAULT_STEP, None);
        assert_eq!(
            pacer.rate_bps(),
            held,
            "post-cut drain window holds the rate"
        );
        pacer.gauge_backoff(true, false, 0.6, DEFAULT_STEP, None);
        assert_eq!(
            probe_ceiling(&pacer),
            Some(armed),
            "oscillation inside the window does not re-arm"
        );
        release_hold(&pacer);

        let mut grew = Vec::new();
        for _ in 0..200 {
            let before = pacer.rate_bps();
            pacer.gauge_backoff(false, true, 0.6, DEFAULT_STEP, None);
            grew.push(pacer.rate_bps() - before);
            if probe_ceiling(&pacer).is_none() {
                break;
            }
        }
        assert!(
            !grew.is_empty() && grew.iter().all(|g| *g > 0.0 && *g <= DEFAULT_STEP),
            "additive: {grew:?}"
        );
        assert!(
            probe_ceiling(&pacer).is_none(),
            "reaching the ceiling releases it"
        );
        let after_release = pacer.rate_bps();
        pacer.gauge_backoff(false, true, 0.6, DEFAULT_STEP, None);
        assert!(
            pacer.rate_bps() > after_release && pacer.rate_bps() > armed,
            "probing continues past the ceiling"
        );
    }

    #[test]
    fn standing_and_draining_queues_past_the_window() {
        let pacer = TransferPacer::adaptive();
        set_rate(&pacer, 1024.0 * 1024.0);
        pacer.gauge_backoff(true, false, 0.6, DEFAULT_STEP, Some(200_000));
        release_hold(&pacer);
        let r_standing = pacer.rate_bps();
        pacer.gauge_backoff(true, false, 0.6, DEFAULT_STEP, Some(200_000));
        let standing_cut = pacer.rate_bps() / r_standing;
        release_hold(&pacer);
        let r_draining = pacer.rate_bps();
        pacer.gauge_backoff(true, false, 0.6, DEFAULT_STEP, Some(120_000));
        let draining_cut = pacer.rate_bps() / r_draining;
        assert!((standing_cut - 0.6).abs() < 1e-6, "cut={standing_cut:.3}");
        assert!((draining_cut - 0.9).abs() < 1e-6, "cut={draining_cut:.3}");
    }

    #[tokio::test]
    async fn an_unset_cap_does_not_throttle() {
        let pacer = TransferPacer::static_cap(0);
        let elapsed = timed_pace(&pacer, 64 * 1024 * 1024, false).await;
        assert!(!pacer.active() && elapsed < 0.5, "elapsed={elapsed:.2}s");
    }

    #[test]
    fn chunk_size_follows_the_rate_within_its_clamps() {
        let pacer = TransferPacer::adaptive();
        set_rate(&pacer, 8.0 * 1024.0 * 1024.0);
        let mid = gauged_chunk_size(&pacer);
        set_rate(&pacer, 1024.0);
        let small = gauged_chunk_size(&pacer);
        set_rate(&pacer, (1u64 << 30) as f64);
        let large = gauged_chunk_size(&pacer);
        assert_eq!(small, TRANSFER_CHUNK_MIN_BYTES);
        assert_eq!(large, TRANSFER_CHUNK_MAX_BYTES);
        assert!(TRANSFER_CHUNK_MIN_BYTES < mid && mid < TRANSFER_CHUNK_MAX_BYTES);
        assert_eq!(
            mid,
            (8.0 * 1024.0 * 1024.0 * TRANSFER_CHUNK_DELAY_BUDGET) as usize
        );
    }

    #[tokio::test]
    async fn verdict_leg_holds_grows_cuts_and_drains() {
        let pacer = TransferPacer::adaptive();
        let r0 = pacer.rate_bps();
        pacer.pace_verdict(1024, None, None).await;
        assert_eq!(pacer.rate_bps(), r0, "a sampleless verdict holds the rate");
        pacer.pace_verdict(1024, Some(false), None).await;
        assert!(pacer.rate_bps() > r0, "a clear verdict grows the rate");
        let before = pacer.rate_bps();
        pacer.pace_verdict(1024, Some(true), None).await;
        assert!(
            pacer.rate_bps() < before,
            "a congested verdict cuts the rate"
        );

        let rate = 2 * 1024 * 1024;
        let pacer = TransferPacer::static_cap(rate as u64);
        let sent = 3 * 1024 * 1024;
        let expected = (sent as f64 - f64::from(rate) * 0.5) / f64::from(rate);
        let elapsed = timed_pace(&pacer, sent, true).await;
        assert!(
            expected * 0.85 <= elapsed && elapsed <= expected * 1.4,
            "elapsed={elapsed:.2}s expected~{expected:.2}s"
        );
    }

    struct FakeSocket {
        pings: Mutex<Vec<Vec<u8>>>,
        fail: AtomicBool,
    }

    impl SessionPing for FakeSocket {
        fn ping(&self, payload: &[u8]) -> bool {
            if self.fail.load(Ordering::Relaxed) {
                return false;
            }
            self.pings.lock().push(payload.to_vec());
            true
        }
    }

    #[test]
    fn gauge_verdicts_come_from_its_own_pings() {
        let socket = Arc::new(FakeSocket {
            pings: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        });
        let clock = Arc::new(UplinkClock::new(socket.clone()));
        let mut gauge = socket_gauge(clock.clone());
        assert_eq!(gauge.sample(), None, "no pong yet gives no verdict");
        let last = socket
            .pings
            .lock()
            .last()
            .cloned()
            .expect("a ping went out");
        clock.note_pong(&last);
        assert_eq!(gauge.sample(), Some(false), "a prompt pong reads clear");
        gauge.last_ping = f64::NEG_INFINITY;
        gauge.sample();
        let late = socket.pings.lock().last().cloned().expect("a second ping");
        clock.backdate_pending(
            &late,
            (f64::from(UplinkGauge::INFLATION_US) + 10_000.0) / 1e6,
        );
        clock.note_pong(&late);
        assert_eq!(
            gauge.sample(),
            Some(true),
            "an inflated round trip reads congested"
        );
        let seq_before = clock.seq();
        clock.note_pong(b"never-sent");
        assert_eq!(clock.seq(), seq_before);
        assert_eq!(
            gauge.sample(),
            None,
            "a pong the clock never sent is ignored"
        );
        socket.fail.store(true, Ordering::Relaxed);
        gauge.last_ping = f64::NEG_INFINITY;
        gauge.sample();
        assert!(
            !gauge.alive(),
            "a dead session socket takes the gauge with it"
        );
    }

    fn fake_clock() -> (Arc<FakeSocket>, Arc<UplinkClock>) {
        let socket = Arc::new(FakeSocket {
            pings: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        });
        let clock = Arc::new(UplinkClock::new(socket.clone()));
        (socket, clock)
    }

    fn last_ping(socket: &FakeSocket) -> Vec<u8> {
        socket
            .pings
            .lock()
            .last()
            .cloned()
            .expect("a ping went out")
    }

    /// The first pong of a transfer's own gauge, back 1.1 s late: behind the megabyte the
    /// upload has already put in the client's first hop.
    fn first_pong_behind_the_upload(socket: &FakeSocket, clock: &Arc<UplinkClock>) -> UplinkGauge {
        let mut gauge = socket_gauge(clock.clone());
        assert_eq!(gauge.sample(), None);
        let first = last_ping(socket);
        clock.backdate_pending(&first, 1.1);
        clock.note_pong(&first);
        gauge
    }

    #[test]
    fn a_floor_kept_between_transfers_is_what_a_transfer_is_read_against() {
        let (socket, clock) = fake_clock();
        assert_eq!(clock.floor_s(), None);
        // The socket's own beat: one ping, and none again until the interval has run.
        assert!(clock.keep_floor());
        assert!(clock.keep_floor());
        assert_eq!(socket.pings.lock().len(), 1, "one ping an interval");
        clock.note_pong(&last_ping(&socket));
        let floor = clock.floor_s().expect("the pong left a floor");
        assert!(floor < 0.05, "{floor}");
        clock.idle_for(UplinkGauge::FLOOR_PING_INTERVAL);
        assert!(clock.keep_floor());
        assert_eq!(socket.pings.lock().len(), 2, "the next interval's ping");
        // A transfer begins, and its queue is over the floor from before it.
        let mut gauge = first_pong_behind_the_upload(&socket, &clock);
        assert_eq!(gauge.sample(), Some(true));
        assert!(gauge.inflation_us() > 1_000_000, "{}", gauge.inflation_us());
        assert!(
            clock.floor_s().expect("a floor") < 0.05,
            "the floor is the path's still"
        );
        // A clock that never pinged has only the transfer's own queue for a floor.
        let (socket, clock) = fake_clock();
        let mut gauge = first_pong_behind_the_upload(&socket, &clock);
        assert_eq!(gauge.sample(), Some(false));
        // A socket that is gone says so to its beat too.
        socket.fail.store(true, Ordering::Relaxed);
        clock.idle_for(UplinkGauge::FLOOR_PING_INTERVAL);
        assert!(!clock.keep_floor());
    }

    fn set_used(pacer: &TransferPacer, bps: f64) {
        pacer.inner.lock().used_bps = Some(bps);
    }

    #[test]
    fn an_allowance_the_transfers_do_not_fill_is_not_raised() {
        let pacer = TransferPacer::adaptive();
        let start = pacer.rate_bps();
        // The transfers moved a tenth of it, or three quarters: clear verdicts say nothing
        // of the rest.
        for moved in [0.1, 0.75] {
            set_used(&pacer, moved * start);
            for _ in 0..8 {
                pacer.gauge_backoff(false, true, 0.65, DEFAULT_STEP, None);
            }
            assert_eq!(pacer.rate_bps(), start, "{moved} of it moved");
        }
        // Filled, as an allowance just doubled is by the next verdict where the sender keeps
        // up (nine tenths), it is raised as it was: doubled before the first congestion.
        set_used(&pacer, 0.9 * start);
        pacer.gauge_backoff(false, true, 0.65, DEFAULT_STEP, None);
        assert_eq!(pacer.rate_bps(), 2.0 * start);
        // Slow start against a path that takes 750 kB/s ends within twice that, where it
        // ran to a hundred times it.
        set_used(&pacer, 750_000.0);
        for _ in 0..12 {
            pacer.gauge_backoff(false, true, 0.65, DEFAULT_STEP, None);
        }
        assert!(pacer.rate_bps() >= 750_000.0, "{}", pacer.rate_bps());
        assert!(pacer.rate_bps() <= 2.0 * 750_000.0, "{}", pacer.rate_bps());
        // A filled allowance is cut as it was.
        let before = pacer.rate_bps();
        set_used(&pacer, before);
        pacer.gauge_backoff(true, false, 0.65, DEFAULT_STEP, None);
        assert!((pacer.rate_bps() - 0.65 * before).abs() < 1.0);
    }

    #[test]
    fn a_cut_takes_an_unfilled_allowance_down_to_what_was_moved() {
        let pacer = TransferPacer::adaptive();
        let at = |rate: f64| {
            assert!(
                (pacer.rate_bps() - rate).abs() < 1.0,
                "{}",
                pacer.rate_bps()
            )
        };
        // Slow start left the allowance at 1 MB/s over a path that carried 600 kB/s: the cut
        // is to what was carried, under the 650 kB/s it would have left.
        set_rate(&pacer, 1_000_000.0);
        set_used(&pacer, 600_000.0);
        pacer.gauge_backoff(true, false, 0.65, DEFAULT_STEP, None);
        at(600_000.0);
        // In the hold after, a reading of what stood in the socket having cleared, the path
        // shows 400 kB/s: the allowance comes down to it once, and no further than a cut.
        set_used(&pacer, 400_000.0);
        pacer.gauge_backoff(true, false, 0.65, DEFAULT_STEP, None);
        at(400_000.0);
        set_used(&pacer, 100_000.0);
        pacer.gauge_backoff(true, false, 0.65, DEFAULT_STEP, None);
        at(400_000.0);
        // Transfers that moved nothing take the next cut no further than two cuts' worth.
        release_hold(&pacer);
        set_used(&pacer, 0.0);
        pacer.gauge_backoff(true, false, 0.65, DEFAULT_STEP, None);
        at(400_000.0 * 0.65 * 0.65);
        // And in the hold after, one cut more at the most.
        pacer.gauge_backoff(true, false, 0.65, DEFAULT_STEP, None);
        at(400_000.0 * 0.65 * 0.65 * 0.65);
        pacer.gauge_backoff(true, false, 0.65, DEFAULT_STEP, None);
        at(400_000.0 * 0.65 * 0.65 * 0.65);
    }

    #[tokio::test]
    async fn the_transfers_own_rate_is_read_from_what_they_drain() {
        let pacer = TransferPacer::adaptive();
        assert_eq!(pacer.inner.lock().used_bps, None);
        pacer.pace(8192).await;
        assert_eq!(
            pacer.inner.lock().used_bps,
            None,
            "nothing to read a rate over yet"
        );
        // Half the window on, what was drained before now is read over the time it took.
        let half = TransferPacer::USED_SECONDS / 2.0;
        tokio::time::sleep(Duration::from_secs_f64(half + 0.05)).await;
        pacer.pace(8192).await;
        let used = pacer
            .inner
            .lock()
            .used_bps
            .expect("half the window was read");
        let most = 8192.0 / half;
        assert!(used > 0.5 * most && used <= most, "{used} of {most}");
        // A pause longer than the window is time the transfers moved nothing in.
        let pause = TransferPacer::USED_SECONDS + 0.05;
        tokio::time::sleep(Duration::from_secs_f64(pause)).await;
        pacer.pace(8192).await;
        let after = pacer.inner.lock().used_bps.expect("the pause was read");
        assert!(after <= 8192.0 / pause, "{after} after {used}");
        // One as long as the restart's is a new transfer, read afresh.
        pacer.inner.lock().ts -= TransferPacer::RESTART_SECONDS;
        pacer.pace(8192).await;
        assert_eq!(pacer.inner.lock().used_bps, None, "the reading starts over");
    }

    #[tokio::test]
    async fn an_allowance_is_not_raised_across_a_pause_in_its_transfers() {
        let pacer = TransferPacer::adaptive();
        set_rate(&pacer, 1_000_000.0);
        // A download read by a slow client in large pieces: a piece, then a pause longer than
        // the window while the client reads, then the next.
        pacer.pace(8192).await;
        tokio::time::sleep(Duration::from_secs_f64(TransferPacer::USED_SECONDS + 0.05)).await;
        pacer.pace(8192).await;
        for _ in 0..4 {
            pacer.gauge_backoff(false, true, 0.65, DEFAULT_STEP, None);
        }
        assert_eq!(
            pacer.rate_bps(),
            1_000_000.0,
            "clear verdicts on an unfilled allowance"
        );
    }

    #[test]
    fn an_uploads_window_is_its_allowance_over_a_round_trip_and_a_chunks_budget() {
        // 6 Mbit/s over a path of 20 ms: 750 kB/s for 32.5 ms.
        assert_eq!(upload_window(750_000.0, 0.02, 1460), 24_375);
        // On a path with no round trip to speak of, the chunk budget alone.
        assert_eq!(upload_window(750_000.0, 0.0, 1460), 9_375);
        // Never under two segments, a modem's or a loopback's.
        assert_eq!(upload_window(6_000.0, 0.0, 1460), 2_920);
        assert_eq!(upload_window(750_000.0, 0.0, 65_483), 130_966);
        // A gigabit path over 20 ms keeps its megabytes.
        assert!(upload_window(125_000_000.0, 0.02, 1460) > 4_000_000);
        // No rate overflows the option.
        assert_eq!(upload_window(f64::MAX, 1.0, 1460), i32::MAX as u32);
    }

    #[test]
    fn an_uploads_socket_follows_its_allowance_and_is_given_back() {
        use std::os::fd::AsRawFd;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        // An Ethernet path's segments: a loopback's own are 64 KiB, and two of them more
        // than the windows read here.
        assert!(set_socket_option(
            listener.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_MAXSEG,
            1460
        ));
        let near = std::net::TcpStream::connect(listener.local_addr().expect("its address"))
            .expect("a connection");
        let (far, _) = listener.accept().expect("its far end");
        let fd = far.as_raw_fd();
        let tcp = |option| socket_option(fd, libc::IPPROTO_TCP, option);
        let buffer = || socket_option(fd, libc::SOL_SOCKET, libc::SO_RCVBUF).expect("a buffer");
        let locked =
            || socket_option(fd, libc::SOL_SOCKET, SO_BUF_LOCK).map(|l| l & SOCK_RCVBUF_LOCK);
        let was = (tcp(libc::TCP_WINDOW_CLAMP).expect("a TCP socket"), buffer());
        let mss = u32::try_from(tcp(libc::TCP_MAXSEG).expect("its segment size")).expect("a size");
        assert!(mss <= 1460, "{mss}");
        let (_socket, clock) = fake_clock();
        let mut gauge = socket_gauge(clock.clone()).windowed(Some(fd));
        // 6 Mbit/s: the buffer is the window (the kernel keeps twice what it is asked for),
        // sized by the process, and the clamp says the same.
        gauge.follow(750_000.0);
        let own = gauge
            .window
            .as_ref()
            .and_then(Window::least_round_trip)
            .expect("the handshake's round trip");
        assert!(own < 0.05, "{own}");
        let window = i32::try_from(upload_window(750_000.0, own, mss)).expect("a window");
        let first = window;
        assert_eq!(buffer(), 2 * window);
        assert_eq!(locked(), Some(SOCK_RCVBUF_LOCK));
        assert_eq!(tcp(libc::TCP_WINDOW_CLAMP), Some(window));
        // A move under an eighth is not worth a call; a larger one is followed.
        gauge.follow(800_000.0);
        assert_eq!(buffer(), 2 * window);
        gauge.follow(1_500_000.0);
        let window = i32::try_from(upload_window(1_500_000.0, own, mss)).expect("a window");
        assert_eq!(buffer(), 2 * window);
        // The kernel's own tuning moves the clamp with the buffer: the next piece puts it back.
        assert!(set_socket_option(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_WINDOW_CLAMP,
            8 << 20
        ));
        gauge.follow(1_500_000.0);
        assert_eq!(tcp(libc::TCP_WINDOW_CLAMP), Some(window));
        // Past what the kernel lets a process size a buffer to, the socket is the kernel's
        // again, its clamp as found, and is not asked again while the allowance stays there.
        gauge.follow(1e12);
        assert_eq!(locked(), Some(0));
        assert_eq!(tcp(libc::TCP_WINDOW_CLAMP), Some(was.0));
        let left = buffer();
        assert!(set_socket_option(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_WINDOW_CLAMP,
            8 << 20
        ));
        gauge.follow(2e12);
        assert_eq!((locked(), buffer()), (Some(0), left));
        assert_eq!(tcp(libc::TCP_WINDOW_CLAMP), Some(8 << 20));
        // An allowance that fits again is held again.
        gauge.follow(750_000.0);
        assert_eq!(locked(), Some(SOCK_RCVBUF_LOCK));
        assert_eq!(buffer(), 2 * first);
        assert_eq!(tcp(libc::TCP_WINDOW_CLAMP), Some(first));
        // An allowance past it again hands the socket back where the last hold sized it.
        gauge.follow(2e12);
        assert_eq!((locked(), buffer()), (Some(0), 2 * first));
        // Given back: the buffer as it was, or as large as a process may size one, the
        // kernel sizing it again, and the clamp as it was found.
        gauge.release_window();
        let most: libc::c_int = std::fs::read_to_string("/proc/sys/net/core/rmem_max")
            .expect("the kernel's limit")
            .trim()
            .parse()
            .expect("a size");
        assert_eq!(locked(), Some(0));
        assert_eq!(buffer(), 2 * (was.1 / 2).min(most));
        assert_eq!(tcp(libc::TCP_WINDOW_CLAMP), Some(was.0));
        gauge.follow(750_000.0);
        assert_eq!(locked(), Some(0), "released is released");
        // No socket, or one that is not TCP, leaves the gauge to its reads alone.
        let mut plain = socket_gauge(clock.clone()).windowed(None);
        plain.follow(1_000_000.0);
        let (unix, _other) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
        let mut plain = socket_gauge(clock).windowed(Some(unix.as_raw_fd()));
        plain.follow(1_000_000.0);
        assert!(plain.window.is_none());
        drop(near);
    }

    /// A session socket whose stream tells what its way back holds.
    struct StampedSocket {
        pings: Mutex<Vec<Vec<u8>>>,
        back: Mutex<Option<f64>>,
    }

    impl SessionPing for StampedSocket {
        fn ping(&self, payload: &[u8]) -> bool {
            self.pings.lock().push(payload.to_vec());
            true
        }

        fn way_back(&self) -> Option<f64> {
            *self.back.lock()
        }
    }

    #[test]
    fn an_upload_is_read_against_its_way_back_where_the_socket_tells_it() {
        let socket = Arc::new(StampedSocket {
            pings: Mutex::new(Vec::new()),
            back: Mutex::new(Some(0.0)),
        });
        let clock = Arc::new(UplinkClock::new(socket.clone()));
        // The path's floor, from before the transfers.
        assert!(clock.keep_floor());
        let first = socket.pings.lock().last().cloned().expect("a ping");
        clock.note_pong(&first);
        // The next pong of each gauge comes back `late` seconds over that floor.
        let pong = |gauge: &mut UplinkGauge, late: f64| {
            gauge.last_ping = f64::NEG_INFINITY;
            // Its ping goes out, and what another gauge's pong left it to read is read.
            let _ = gauge.sample();
            let ping = socket.pings.lock().last().cloned().expect("a ping");
            clock.backdate_pending(&ping, late);
            clock.note_pong(&ping);
            gauge.sample()
        };
        let mut upload = socket_gauge(clock.clone()).for_upload();
        let mut download = socket_gauge(clock.clone());
        // The stream's steer stands 60 ms of queue on the way out, the way back free: that
        // queue is the download's to read and none of the upload's.
        assert_eq!(pong(&mut upload, 0.06), Some(false));
        assert!(upload.inflation_us() < 1_000, "{}", upload.inflation_us());
        assert_eq!(pong(&mut download, 0.06), Some(true));
        // The way back holds 40 ms and the round trip is that much longer: the upload's own
        // queue.
        *socket.back.lock() = Some(0.04);
        assert_eq!(pong(&mut upload, 0.045), Some(true));
        assert_eq!(upload.inflation_us(), 40_000);
        // The stamps say 60 ms with the round trip at its floor: two clocks that drifted
        // apart, and no queue.
        *socket.back.lock() = Some(0.06);
        assert_eq!(pong(&mut upload, 0.0), Some(false));
        assert_eq!(pong(&mut download, 0.0), Some(false));
        // No stamp to say: the round trip, as for a socket that never tells.
        *socket.back.lock() = None;
        assert_eq!(pong(&mut upload, 0.06), Some(true));
        assert_eq!(pong(&mut upload, 0.0), Some(false));
    }

    /// A session socket whose stream has written what the test says.
    struct StreamingSocket {
        written: AtomicU64,
    }

    impl SessionPing for StreamingSocket {
        fn ping(&self, _payload: &[u8]) -> bool {
            true
        }

        fn stream_bytes(&self) -> Option<u64> {
            Some(self.written.load(Ordering::Relaxed))
        }
    }

    /// A tenth of a second at a time, a download moving 100 kB/s beside a stream writing
    /// `stream_bps`: the share the gauge reads at the end of `seconds`.
    fn share_after(
        gauge: &mut UplinkGauge,
        socket: &StreamingSocket,
        stream_bps: u64,
        seconds: f64,
    ) -> Option<f64> {
        let mut share = None;
        for tick in 0..=(seconds * 10.0).round() as u64 {
            socket
                .written
                .store(tick * stream_bps / 10, Ordering::Relaxed);
            share = gauge.share_bps_at(10_000, 100.0 + tick as f64 / 10.0);
        }
        share
    }

    #[test]
    fn a_downloads_share_is_of_what_it_and_the_stream_moved_together() {
        let socket = Arc::new(StreamingSocket {
            written: AtomicU64::new(0),
        });
        let clock = Arc::new(UplinkClock::new(socket.clone()));
        let mut gauge = socket_gauge(clock.clone()).beside_stream();
        assert_eq!(
            share_after(&mut gauge, &socket, 300_000, 0.4),
            None,
            "under half the window says nothing of either rate"
        );
        assert!(
            gauge.share_unread(),
            "and no congested verdict is the download's yet"
        );
        // 100 kB/s beside 300 kB/s: an even split is 200 kB/s each.
        let mut gauge = socket_gauge(clock.clone()).beside_stream();
        let share = share_after(&mut gauge, &socket, 300_000, 3.0).expect("a share");
        let even = 200_000.0 * BESIDE_SHARE;
        assert!((share - even).abs() < 0.02 * even, "{share} of {even}");
        assert!(!gauge.share_unread());
        // The stream alone moving faster raises it: the split is of the two together.
        let mut gauge = socket_gauge(clock.clone()).beside_stream();
        let share = share_after(&mut gauge, &socket, 500_000, 3.0).expect("a share");
        let even = 300_000.0 * BESIDE_SHARE;
        assert!((share - even).abs() < 0.02 * even, "{share} of {even}");
        // An upload's gauge keeps no share: its bytes go the other way.
        let mut upload = socket_gauge(clock);
        assert_eq!(share_after(&mut upload, &socket, 300_000, 0.2), None);
        assert!(!upload.share_unread(), "and none is awaited");
        assert_eq!(share_after(&mut upload, &socket, 300_000, 3.0), None);
    }

    #[test]
    fn a_socket_that_leaves_the_gauge_takes_none_of_the_split_with_it() {
        let streams = [300_000u64, 300_000];
        let sockets = streams.map(|_| {
            Arc::new(StreamingSocket {
                written: AtomicU64::new(0),
            })
        });
        let clocks = sockets
            .clone()
            .map(|socket| Arc::new(UplinkClock::new(socket)));
        let mut gauge = UplinkGauge::new(clocks).beside_stream();
        // 100 kB/s beside two sockets of 300 kB/s each for three seconds, the second closing
        // at the last half second's start.
        let mut share = None;
        for tick in 0..=30u64 {
            for (socket, bps) in sockets.iter().zip(streams) {
                socket.written.store(tick * bps / 10, Ordering::Relaxed);
            }
            if tick == 25 {
                // As a ping to the closed socket drops it.
                gauge.conns.pop();
            }
            share = gauge.share_bps_at(10_000, 100.0 + tick as f64 / 10.0);
        }
        // Over the last second the download moved 100 kB, the streams 420 kB: both until the
        // second closed, the first alone after.
        let share = share.expect("a share");
        let even = 260_000.0 * BESIDE_SHARE;
        assert!((share - even).abs() < 0.1 * even, "{share} of {even}");
    }

    #[test]
    fn a_download_beside_no_counted_stream_keeps_no_share() {
        let socket = Arc::new(FakeSocket {
            pings: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        });
        let mut gauge = socket_gauge(Arc::new(UplinkClock::new(socket))).beside_stream();
        for tick in 0..30 {
            assert_eq!(
                gauge.share_bps_at(10_000, 100.0 + f64::from(tick) / 10.0),
                None
            );
            assert!(!gauge.share_unread(), "no share is awaited");
        }
    }

    #[test]
    fn a_download_beside_a_stream_backs_off_no_further_than_its_share() {
        let pacer = TransferPacer::adaptive();
        let share = Some(340_000.0);
        // The share stands over what the cut would leave: the cut stops at it.
        set_rate(&pacer, 400_000.0);
        pacer.gauge_backoff_beside(true, false, 0.65, DEFAULT_STEP, Some(40_000), share);
        assert!(
            (pacer.rate_bps() - 340_000.0).abs() < 1.0,
            "{}",
            pacer.rate_bps()
        );
        // A download under its share is cut no lower: the verdict on the steer's queue raises
        // it a step, as a clear one would, and up to the share and no further.
        release_hold(&pacer);
        set_rate(&pacer, 200_000.0);
        pacer.gauge_backoff_beside(true, false, 0.65, DEFAULT_STEP, Some(40_000), share);
        assert!(
            (pacer.rate_bps() - 200_000.0 - DEFAULT_STEP).abs() < 1.0,
            "{}",
            pacer.rate_bps()
        );
        for _ in 0..40 {
            pacer.gauge_backoff_beside(true, false, 0.65, DEFAULT_STEP, Some(40_000), share);
        }
        assert!(
            (pacer.rate_bps() - 340_000.0).abs() < 1.0,
            "{}",
            pacer.rate_bps()
        );
        // Unless the transfers leave what they have unfilled.
        set_rate(&pacer, 200_000.0);
        set_used(&pacer, 50_000.0);
        pacer.gauge_backoff_beside(true, false, 0.65, DEFAULT_STEP, Some(40_000), share);
        assert!(
            (pacer.rate_bps() - 200_000.0).abs() < 1.0,
            "{}",
            pacer.rate_bps()
        );
        pacer.inner.lock().used_bps = None;
        // One with far more than its share, as a slow start leaves it, is taken down to it,
        // and follows it down through the hold after: the share is read off what the two
        // wrote, the download's own excess in it at first.
        release_hold(&pacer);
        set_rate(&pacer, 1_000_000.0);
        pacer.gauge_backoff_beside(true, false, 0.65, DEFAULT_STEP, Some(40_000), share);
        assert!(
            (pacer.rate_bps() - 340_000.0).abs() < 1.0,
            "{}",
            pacer.rate_bps()
        );
        pacer.gauge_backoff_beside(
            true,
            false,
            0.65,
            DEFAULT_STEP,
            Some(40_000),
            Some(300_000.0),
        );
        assert!(
            (pacer.rate_bps() - 300_000.0).abs() < 1.0,
            "{}",
            pacer.rate_bps()
        );
    }

    #[tokio::test]
    async fn a_queue_past_a_steers_own_is_yielded_to_in_full() {
        let share = Some(340_000.0);
        let pacer = TransferPacer::adaptive();
        set_rate(&pacer, 400_000.0);
        pacer
            .pace_verdict_beside(1024, Some(true), Some(BESIDE_INFLATION_US), share)
            .await;
        assert!(
            (pacer.rate_bps() - 340_000.0).abs() < 1.0,
            "a queue up to the line is the steer's to drain: {}",
            pacer.rate_bps()
        );
        let pacer = TransferPacer::adaptive();
        set_rate(&pacer, 400_000.0);
        pacer
            .pace_verdict_beside(1024, Some(true), Some(BESIDE_INFLATION_US + 1), share)
            .await;
        assert!(
            (pacer.rate_bps() - 260_000.0).abs() < 1.0,
            "past it the path has gone late: {}",
            pacer.rate_bps()
        );
    }

    /// A session socket whose peer answers every ping the moment it goes out.
    struct AnsweringSocket {
        clock: OnceLock<std::sync::Weak<UplinkClock>>,
    }

    impl SessionPing for AnsweringSocket {
        fn ping(&self, payload: &[u8]) -> bool {
            if let Some(clock) = self.clock.get().and_then(std::sync::Weak::upgrade) {
                clock.note_pong(payload);
            }
            true
        }
    }

    #[tokio::test]
    async fn a_chunk_held_at_once_grows_the_allowance_at_the_ping_cadence() {
        let socket = Arc::new(AnsweringSocket {
            clock: OnceLock::new(),
        });
        let clock = Arc::new(UplinkClock::new(socket.clone()));
        socket
            .clock
            .set(Arc::downgrade(&clock))
            .expect("the clock is set once");
        let mut gauge = socket_gauge(clock);
        let pacer = TransferPacer::adaptive();
        let start = pacer.rate_bps();
        // What the HTTP stack hands an upload at once on a fast link.
        assert!(pace_gauged(&pacer, &mut gauge, 408 * 1024).await);
        assert!(
            pacer.rate_bps() >= 4.0 * start,
            "a clear path doubles the allowance at each ping while the chunk drains, \
             not once for the chunk: {} -> {}",
            start,
            pacer.rate_bps()
        );
    }

    #[tokio::test]
    async fn a_gauge_left_without_sockets_stops_pacing() {
        let socket = Arc::new(FakeSocket {
            pings: Mutex::new(Vec::new()),
            fail: AtomicBool::new(true),
        });
        let mut gauge = socket_gauge(Arc::new(UplinkClock::new(socket)));
        assert!(!pace_gauged(&TransferPacer::adaptive(), &mut gauge, 1 << 20).await);
    }

    #[test]
    fn the_floor_holds_through_jitter_and_ages_out_a_route_change() {
        let mut floor = FloorHistory::new();
        floor.observe(5_000, 0.0);
        assert_eq!(floor.observe(9_000, 1.0), 5_000);
        let mut current = 0;
        for i in 1..=UplinkGauge::FLOOR_BUCKETS {
            current = floor.observe(20_000, i as f64 * UplinkGauge::FLOOR_BUCKET_SECONDS);
        }
        assert_eq!(current, 20_000, "a route change re-baselines the floor");
    }
}
