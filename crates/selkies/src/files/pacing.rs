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
//! answer pings in the network process, so no page cooperation is needed. The rejected
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
//! standing queue. Ping payloads are per-session counters; the pong echoes its ping's payload
//! (RFC 6455), so [`UplinkClock::note_pong`] matches replies to send times and stray pongs
//! fall through unmatched. A ping failure drops that socket from the gauge; a gauge with no
//! sockets left reports not alive and the caller stops pacing, since a vanished session leaves
//! nothing to protect.

use std::collections::VecDeque;
use std::net::IpAddr;
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

/// A session socket the gauge can ping: the transport implements it over the socket's writer.
pub trait SessionPing: Send + Sync {
    /// Send a WebSocket ping carrying `payload` behind everything written to the socket so
    /// far; `false` when the socket is gone, which drops it from every gauge.
    fn ping(&self, payload: &[u8]) -> bool;
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
            state.pending.push_back((payload, monotonic()));
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
        let rtt = ((monotonic() - sent) * 1e6).max(0.0) as u32;
        state.rtt_us = Some(rtt);
        state.seq += 1;
        state.answered = Some(state.answered.map_or(sent, |a| a.max(sent)));
    }

    /// The round trip last measured, in milliseconds rounded to a tenth, for `/api/sessions`.
    pub fn rtt_ms(&self) -> Option<f64> {
        let rtt = self.state.lock().rtt_us?;
        if rtt == 0 {
            return None;
        }
        Some((rtt as f64 / 100.0).round() / 10.0)
    }

    /// When the newest answered ping went out, on the [`monotonic`] clock: everything
    /// written to the socket before it has reached the peer.
    pub fn answered(&self) -> Option<f64> {
        self.state.lock().answered
    }

    fn seq(&self) -> u64 {
        self.state.lock().seq
    }

    /// The fresh RTT past `last_seq`, folded into the floor: `(rtt_us, floor_us, seq)`.
    fn fresh(&self, last_seq: u64, now: f64) -> Option<(u32, u32, u64)> {
        let mut state = self.state.lock();
        if state.seq == last_seq {
            return None;
        }
        let rtt = state.rtt_us?;
        let seq = state.seq;
        let floor = state.floor.observe(rtt, now);
        Some((rtt, floor, seq))
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

/// Congestion verdicts for one client's bulk transfer, timed end to end over that client's
/// own session socket(s) (module docs).
pub struct UplinkGauge {
    conns: Vec<(Arc<UplinkClock>, u64)>,
    last_ping: f64,
    inflation_us: u32,
}

impl UplinkGauge {
    /// Seconds between pings on each gauged socket.
    pub const PING_INTERVAL: f64 = 0.2;
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
                .map(|c| (c.seq(), c))
                .map(|(s, c)| (c, s))
                .collect(),
            last_ping: f64::NEG_INFINITY,
            inflation_us: 0,
        }
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
    /// holds its rate).
    pub fn sample(&mut self) -> Option<bool> {
        let now = monotonic();
        if now - self.last_ping >= Self::PING_INTERVAL {
            self.last_ping = now;
            self.conns.retain(|(clock, _)| clock.ping_behind());
        }
        let mut verdict: Option<bool> = None;
        let mut inflation = 0u32;
        for (clock, last) in &mut self.conns {
            let Some((rtt, floor, seq)) = clock.fresh(*last, now) else {
                continue;
            };
            *last = seq;
            let congested = rtt > floor.saturating_add(Self::INFLATION_US);
            inflation = inflation.max(rtt.saturating_sub(floor));
            verdict = Some(verdict.unwrap_or(false) || congested);
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
        if !self.active() {
            return;
        }
        if self.adaptive
            && let Some(congested) = congested
        {
            let step = (8.0 * 1024.0f64).max((self.rate_bps() * 0.03).floor());
            self.gauge_backoff(congested, !congested, 0.65, step, inflation_us);
        }
        if let Some(wait) = self.bucket(nbytes) {
            tokio::time::sleep(wait).await;
        }
    }

    /// Drain `nbytes` from the token bucket; the overdraw to sleep off, if any.
    fn bucket(&self, nbytes: usize) -> Option<Duration> {
        let now = monotonic();
        let mut s = self.inner.lock();
        if self.adaptive && now - s.ts > 10.0 {
            s.slow_start = true;
        }
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
    fn gauge_backoff(
        &self,
        congested: bool,
        clear: bool,
        cut: f64,
        step: f64,
        inflation_us: Option<u32>,
    ) {
        let mut s = self.inner.lock();
        if congested {
            s.slow_start = false;
            let now = monotonic();
            let draining =
                matches!((inflation_us, s.last_inflation_us), (Some(i), Some(l)) if i < l);
            s.last_inflation_us = inflation_us;
            let mut cut = cut;
            if !s.congested {
                s.congested = true;
                s.probe_ceiling = Some(s.rate_bps.max(2.0 * Self::RATE_FLOOR));
            } else if now < s.hold_until {
                return;
            } else if draining {
                cut = 1.0 - (1.0 - cut) / 4.0;
            }
            s.rate_bps = (s.rate_bps * cut).max(Self::RATE_FLOOR);
            s.hold_until = now + Self::HOLD_SECONDS;
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
        s.rate_bps = if s.slow_start {
            (s.rate_bps * 2.0).min(bound)
        } else {
            (s.rate_bps + step).min(bound)
        };
    }
}

#[cfg(test)]
mod tests {
    //! The bucket delivers the configured rate, the congestion step arms its ceiling once per epoch
    //! and probes multiplicatively only before the first congestion, an unset cap does not
    //! throttle, chunks follow the rate, and the gauge's verdicts come from timing its own pings.

    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

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
        let expected = (sent as f64 - rate as f64 * 0.5) / rate as f64;
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
        let expected = (sent as f64 - rate as f64 * 0.5) / rate as f64;
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
        clock.backdate_pending(&late, (UplinkGauge::INFLATION_US as f64 + 10_000.0) / 1e6);
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
