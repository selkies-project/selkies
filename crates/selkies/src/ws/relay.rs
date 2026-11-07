/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Bounded video delivery for one (client, display) pair, and the frames every page holds.
//!
//! The fan-out offers every encoded chunk synchronously and never awaits a socket; each
//! relay's own task drains its backlog. One slow client can therefore neither pace the other
//! clients nor back frames up into the shared pipeline: past its byte budget
//! (`VIDEO_RELAY_BUDGET_SECONDS` of stream at the configured bitrate) it drops its backlog.
//! Key frames are exempt from the budget (part of one is useless), so the true bound is the
//! budget plus one key frame burst. That budget is the only bound on a slow client: a send
//! waits for as long as its socket keeps draining, and only a socket that stops draining
//! costs the client its connection (`Client::send_live`).
//!
//! Where the encoder names the frame each one predicts from (a whole-frame session; the wire
//! header's last field), a page that does not own its display is served as a selective
//! forwarding unit serves a receiver on a narrow link: one stream for every page, each sent
//! what its link carries. A delta frame finding `VIDEO_RELAY_ROOM_FRAMES` of the page's frames
//! still on their way out of this host (`frames_ahead`), or its queue `VIDEO_RELAY_LAG_SECONDS`
//! past the path's round trip (`lag`, read end to end from a ping written behind the frames),
//! is left out, which opens a run; the frames predicting from it are held back, and once the
//! page has room the encoder is told the run's first frame is lost to it (`RelayHost::relay_lost`,
//! the client's own `LOST_FRAME`). Its next frame predicts from the last frame the page was
//! sent and goes out, so the page keeps a frame rate its link carries and the shared stream
//! carries no key frame for it. A run past `VIDEO_RELAY_REACH_FRAMES` is reported without
//! waiting for room, while the encoder still holds the frame to predict from. Only a run the
//! encoder has not predicted past in `VIDEO_RELAY_LOST_RECOVERY_SECONDS` falls back to the
//! key frame gate; elsewhere a drop skips ahead to the next key frame.
//!
//! Each frame a relay writes is one its page holds (`CommonFrames`, joined at a key frame and
//! left at the gate), and the display's encoder is told of each frame every page holds. An
//! encoder that keeps long-term references then pins the newest such anchor, so a run of any
//! depth is predicted past on its report, and flags an anchor predicting from such a frame
//! (`FRAME_ANCHOR`), which every page decodes: it goes to a page without room as a key frame
//! does, unless the page is too far behind for one, and ends the page's run.
//!
//! The read of the page's queue moves its floor onto a least round trip that stood
//! `VIDEO_RELAY_LAG_SECONDS` or more past it through `VIDEO_RELAY_SETTLE_SECONDS` of pongs (a
//! queue other traffic keeps standing on the page's path, or a route onto a longer one), and a
//! lower round trip takes it back down; a queue of the page's own drains within that span while
//! the relay leaves its frames out, so it never settles there.
//!
//! The owner's relay also writes the padding its display's path probe asks for
//! (`set_padding`): `0x07` chunks of `PAD_STEP_S` of the probe's rate, written only while no
//! frame waits, so a frame waits behind one chunk at most, each counted in what the display
//! sent (`RelayHost::note_padding`), with a ping behind a chunk every `PAD_PING_S` while no frame
//! awaits its ack, whose pong acks the bytes sent through it for the steer. A tenth of a second
//! keeps the pings the uplink clock holds (`PENDING_MAX`) over two and a half seconds of queue.
//!
//! Reference-chain safety is tracked per stripe row (the header's `stripe_y`): one capture
//! frame can mix IDR and delta stripes, so after any drop a row's delta chunks stay gated
//! until that row's own IDR arrives. JPEG chunks have no reference chain: never gated, and a
//! drop only costs a repaint request. A fresh relay starts fully gated, so a joining client
//! waits for a key frame instead of decoding mid-GOP garbage.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bytes::Bytes;
use log::info;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::core::DisplayId;
use crate::display::capture::{CommonFrames, FRAME_ANCHOR, FRAME_NUM_WRAP, LOST_FRAME_MEMORY};
use crate::ws::client::{Client, ClientId, Frame};
use crate::ws::link::{DisplayLink, RTT_SAMPLE_SANE_MAX_MS};
use crate::ws::now;
use crate::ws::protocol::{OPCODE_PADDING, OPCODE_VIDEO};
use crate::ws::steer::ConnectionVerdict;

/// Per-client video backlog bound as seconds of stream at the configured bitrate, floored so
/// low-bitrate streams still absorb transport jitter.
pub const VIDEO_RELAY_BUDGET_SECONDS: f64 = 2.0;
pub const VIDEO_RELAY_BUDGET_MIN_BYTES: usize = 4 * 1024 * 1024;
/// Floor between one relay's key frame re-requests while gated: pixelflux collapses concurrent
/// requests into one flag, so this only bounds the IDR bitrate a hopeless client can add.
pub const VIDEO_RELAY_SYNC_FLOOR_SECONDS: f64 = 1.0;
/// How long a relay holds back a client's frames that predict from frames it dropped, waiting
/// for the encoder to predict past them, before it asks for a key frame instead; the page's
/// own decode gate waits as long.
pub const VIDEO_RELAY_LOST_RECOVERY_SECONDS: f64 = 1.0;
/// A page that does not own its display is sent a delta frame only while fewer than this many
/// of its frames are still on their way out of this host, and while the oldest ping written
/// behind its frames has gone unanswered no longer than the round trip and this lag.
pub const VIDEO_RELAY_ROOM_FRAMES: usize = 3;
pub const VIDEO_RELAY_LAG_SECONDS: f64 = 0.1;
pub const VIDEO_RELAY_PING_SECONDS: f64 = 0.05;
/// How long a page's least round trip stands `VIDEO_RELAY_LAG_SECONDS` past its floor before it
/// becomes the floor.
pub const VIDEO_RELAY_SETTLE_SECONDS: f64 = 3.0;
/// Padding: the stretch of the probe's rate one chunk carries, how often a chunk has a ping
/// behind it, and the chunk's bounds in bytes.
pub const PAD_STEP_S: f64 = 0.02;
pub const PAD_PING_S: f64 = 0.1;
pub const PAD_CHUNK_MIN: usize = 1200;
pub const PAD_CHUNK_MAX: usize = 256 * 1024;

/// The largest padding chunk; each chunk is a slice of it.
static PADDING: LazyLock<Bytes> = LazyLock::new(|| {
    let mut chunk = vec![0u8; PAD_CHUNK_MAX];
    chunk[0] = OPCODE_PADDING;
    Bytes::from(chunk)
});
/// Frames a run may span before the encoder is told without waiting for the page's room.
pub const VIDEO_RELAY_REACH_FRAMES: u32 = 5;
/// A page this far behind is too far to be answered without room: it waits for a key frame.
pub const VIDEO_RELAY_FAR_FRAMES: usize = 12;
pub const VIDEO_RELAY_FAR_SECONDS: f64 = 1.0;
/// How long a run may wait for the page's room where the encoder pins an anchor every page holds.
pub const VIDEO_RELAY_ANCHORED_RECOVERY_SECONDS: f64 = 4.0;
/// An anchor goes to a page without room only while its queue stands under this past the round trip.
pub const VIDEO_RELAY_ANCHOR_SECONDS: f64 = 0.25;
/// How many of the frames a relay sent it remembers, as far back as a frame may predict from.
pub const VIDEO_RELAY_SENT_MEMORY: usize = 4096;
/// How many frames ahead of a possible `frame_num` wrap an open run is reported.
pub const VIDEO_RELAY_WRAP_LEAD: u32 = 2;
/// H.264's id in the wire header's codec nibble.
pub const WIRE_H264: u8 = 1;
/// Bytes of the video wire header.
const VIDEO_HEADER_LEN: usize = 12;

/// One encoded chunk as the fan-out offers it: the stripe's bytes shared with the encoder's
/// buffer, header included.
#[derive(Clone, Debug)]
pub struct RelayItem {
    pub data: Bytes,
    /// The low sixteen bits of pixelflux's frame counter, as the wire and the acks carry it.
    pub frame_id: u16,
    /// When the frame was captured, `CLOCK_MONOTONIC` nanoseconds, zero where unstamped.
    pub capture_ns: i64,
    /// When its encode started, on the same clock.
    pub encode_start_ns: i64,
    /// When its encode ended, on the same clock.
    pub encode_end_ns: i64,
}

/// The fields of a `0x04` header a relay reads.
#[derive(Clone, Copy, Debug)]
struct VideoHeader {
    key: bool,
    anchor: bool,
    codec: u8,
    frame_id: u16,
    row: u16,
    reference: u16,
}

fn video_header(data: &[u8]) -> Option<VideoHeader> {
    if data.len() < VIDEO_HEADER_LEN || data[0] != OPCODE_VIDEO {
        return None;
    }
    Some(VideoHeader {
        key: data[1] & 0x0F == 0x01,
        anchor: data[1] & FRAME_ANCHOR != 0,
        codec: data[1] >> 4,
        frame_id: u16::from_be_bytes([data[2], data[3]]),
        row: u16::from_be_bytes([data[4], data[5]]),
        reference: u16::from_be_bytes([data[10], data[11]]),
    })
}

/// What a relay needs from the server it serves.
pub trait RelayHost: Send + Sync {
    /// Whether `client` is the page that owns `display_id`.
    fn owns_display(&self, client: ClientId, display_id: &str) -> bool;

    /// Tell the encoder a relay left `frame_id` and the frames after it out, as the client's
    /// own `LOST_FRAME` would; a run past its `reach` is taken as a key frame request is (once
    /// a second from a page that does not own the display). False when refused.
    fn relay_lost(&self, client: ClientId, display_id: &str, frame_id: u16, reach: bool) -> bool;

    /// Run `f` over the display's common frames (`held`: written to the socket, else handed
    /// to a relay's queue), which tell the encoder themselves of a frame every page holds.
    fn with_common_frames(
        &self,
        display_id: &str,
        held: bool,
        f: &mut dyn FnMut(&mut CommonFrames<ClientId>),
    );

    /// A frame is being handed to the socket: stamped for the display's owner (`note_send`)
    /// and timed for the stats. Whether a ping is to be written behind the frame.
    fn note_send(&self, client: ClientId, display_id: &str, item: &RelayItem) -> bool;

    /// The page's connection verdict changed; it is told `CONNECTION poor` or `CONNECTION ok`.
    fn connection_verdict(&self, client: ClientId, poor: bool);

    /// The relay's socket died: the client leaves the registry.
    fn socket_died(&self, client: ClientId);

    /// The relay's drain task ended; the host forgets the relay where it still lists this one.
    fn relay_finished(&self, display_id: &str, relay: &Arc<VideoRelay>);

    /// A padding chunk of `size` bytes is being written: counted in what the display sent
    /// (`sent_bytes`, and `pad_bytes` apart), with the link it was counted in and the bytes
    /// sent through it; `None`, and nothing written, where `client` no longer owns the display
    /// or its gate is shut.
    fn note_padding(
        &self,
        client: ClientId,
        display_id: &str,
        size: usize,
    ) -> Option<(Arc<Mutex<DisplayLink>>, u64)>;
}

struct Inner {
    backlog: VecDeque<RelayItem>,
    backlog_bytes: usize,
    live_rows: HashSet<u16>,
    stopped: bool,
    next_sync_req: f64,
    verdict: ConnectionVerdict,
    sent: VecDeque<u16>,
    sent_set: HashSet<u16>,
    lost_run: u32,
    lost_since: f64,
    lost_first: Option<u16>,
    lost_told: bool,
    since_key: u32,
    numbered: bool,
    written: u64,
    marks: VecDeque<u64>,
    gauged: bool,
    pinged: f64,
    pongs: u64,
    rtt_floor: Option<f64>,
    /// The least round trip that stood past the floor through the settle span, and the
    /// round trips of that span, `(time, seconds)`.
    settled_floor: Option<f64>,
    recent_rtts: VecDeque<(f64, f64)>,
    anchored: bool,
    tracked: bool,
    gated: bool,
    lost_gated: bool,
    /// The padding the display's steer asks of this relay, until `pad_until`; when the next
    /// chunk is due, and when one last had a ping behind it.
    pad_bps: f64,
    pad_until: f64,
    pad_next: f64,
    pad_pinged: f64,
}

impl Inner {
    /// The bytes of the padding chunk due at `now`, or none.
    fn padding_due(&mut self, now: f64) -> Option<usize> {
        if self.pad_bps <= 0.0 || now >= self.pad_until || now < self.pad_next {
            return None;
        }
        self.pad_next = (self.pad_next + PAD_STEP_S).max(now - PAD_STEP_S);
        Some(((self.pad_bps * PAD_STEP_S / 8.0) as usize).clamp(PAD_CHUNK_MIN, PAD_CHUNK_MAX))
    }

    fn remember_sent(&mut self, frame_id: u16) {
        if self.sent_set.insert(frame_id) {
            self.sent.push_back(frame_id);
        }
        while self.sent.len() > VIDEO_RELAY_SENT_MEMORY {
            if let Some(old) = self.sent.pop_front() {
                self.sent_set.remove(&old);
            }
        }
    }

    fn forget_sent(&mut self, frame_id: u16) {
        if self.sent_set.remove(&frame_id) {
            self.sent.retain(|f| *f != frame_id);
        }
    }

    fn clear_sent(&mut self) {
        self.sent.clear();
        self.sent_set.clear();
    }

    /// Count a frame lost to this client, starting a run where none is open.
    fn lose(&mut self, frame_id: u16, now: f64) {
        self.lost_run += 1;
        if self.lost_first.is_none() {
            self.lost_first = Some(frame_id);
            self.lost_since = now;
            self.lost_told = false;
        }
    }

    /// The run is over: the encoder predicted past it, or a key frame came.
    fn end_lost(&mut self) {
        self.lost_run = 0;
        self.lost_first = None;
        self.lost_since = 0.0;
        self.lost_told = false;
        self.lost_gated = false;
    }
}

/// Bounded video delivery for one (client, display) pair.
pub struct VideoRelay {
    host: Arc<dyn RelayHost>,
    pub display_id: DisplayId,
    pub client: Arc<Client>,
    pub budget: usize,
    inner: Mutex<Inner>,
    wake: Notify,
}

impl VideoRelay {
    /// A relay for `client` on `display_id` with the skip-ahead `budget`.
    pub fn new(
        host: Arc<dyn RelayHost>,
        display_id: &str,
        client: Arc<Client>,
        budget: usize,
    ) -> Arc<VideoRelay> {
        Arc::new(VideoRelay {
            host,
            display_id: display_id.to_string(),
            client,
            budget,
            inner: Mutex::new(Inner {
                backlog: VecDeque::new(),
                backlog_bytes: 0,
                live_rows: HashSet::new(),
                stopped: false,
                next_sync_req: 0.0,
                verdict: ConnectionVerdict::new(),
                sent: VecDeque::new(),
                sent_set: HashSet::new(),
                lost_run: 0,
                lost_since: 0.0,
                lost_first: None,
                lost_told: false,
                since_key: 0,
                numbered: false,
                written: 0,
                marks: VecDeque::new(),
                gauged: false,
                pinged: 0.0,
                pongs: 0,
                rtt_floor: None,
                settled_floor: None,
                recent_rtts: VecDeque::new(),
                anchored: false,
                tracked: false,
                gated: false,
                lost_gated: false,
                pad_bps: 0.0,
                pad_until: 0.0,
                pad_next: 0.0,
                pad_pinged: 0.0,
            }),
            wake: Notify::new(),
        })
    }

    /// Start the drain task.
    pub fn start(self: &Arc<Self>) {
        let relay = self.clone();
        tokio::spawn(async move { relay.run().await });
    }

    /// Judge windows of `window_s` instead of the default, for the suites that shorten them.
    pub fn set_verdict_window(&self, window_s: f64) {
        self.inner.lock().verdict = ConnectionVerdict::with_window(window_s);
    }

    /// Count one chunk offered or held back and `missed` chunks that will not reach this
    /// client, telling its page when its connection verdict changes.
    fn judge(&self, inner: &mut Inner, missed: u64) {
        if let Some(poor) = inner.verdict.note(1, missed, now()) {
            self.host.connection_verdict(self.client.id, poor);
        }
    }

    /// Graceful: an in-flight send completes; canceling mid-frame would tear the framing on a
    /// socket that stays open for control.
    pub fn stop(&self) {
        {
            let mut inner = self.inner.lock();
            inner.stopped = true;
            inner.backlog.clear();
            inner.backlog_bytes = 0;
        }
        self.leave_common();
        self.wake.notify_one();
    }

    pub fn stopped(&self) -> bool {
        self.inner.lock().stopped
    }

    fn leave_common(&self) {
        let client = self.client.id;
        for held in [false, true] {
            self.host
                .with_common_frames(&self.display_id, held, &mut |common| common.leave(&client));
        }
    }

    fn join_common(&self) {
        let client = self.client.id;
        for held in [false, true] {
            self.host
                .with_common_frames(&self.display_id, held, &mut |common| common.join(client));
        }
    }

    /// This client holds a video frame its relay wrote, or where not `held` was handed one its
    /// relay queued: a key frame, or a delta naming the one it predicts from, which this relay
    /// sends only where the client holds that one.
    fn hold(&self, inner: &Inner, data: &[u8], held: bool) {
        let Some(header) = video_header(data) else {
            return;
        };
        if inner.live_rows.contains(&header.row)
            && (header.key || header.reference != header.frame_id)
        {
            let client = self.client.id;
            self.host
                .with_common_frames(&self.display_id, held, &mut |common| {
                    common.hold(&client, header.frame_id, header.key)
                });
        }
    }

    /// ACK backpressure engaged: drop the undrained backlog and `item`, the chunk held back.
    /// A delivering client of a tracked stream keeps its rows: the frames dropped open a run,
    /// and once the gate lifts it resumes on the frame predicting past it (`ungate`). An
    /// anchor predicting from a frame the client was sent still goes. Otherwise every row is
    /// gated, so the client resumes only at the IDR the lift requests. The chunks count against
    /// the connection unless `counted` is false (a stalled client is no judge of its link).
    pub fn flush_for_gate(&self, counted: bool, item: Option<RelayItem>) {
        let mut inner = self.inner.lock();
        let at = now();
        let backlog_len = inner.backlog.len();
        let mut items: Vec<(RelayItem, bool)> =
            inner.backlog.drain(..).map(|i| (i, false)).collect();
        if let Some(item) = item {
            items.push((item, true));
        }
        let dropped_key = items
            .iter()
            .any(|(i, _)| video_header(&i.data).is_some_and(|h| h.key));
        if counted && inner.tracked && (inner.gated || !inner.live_rows.is_empty()) && !dropped_key
        {
            inner.gated = true;
            let mut kept = Vec::new();
            for (it, is_item) in items {
                let Some(header) = video_header(&it.data) else {
                    continue;
                };
                if header.anchor
                    && header.reference != header.frame_id
                    && inner.sent_set.contains(&header.reference)
                {
                    if is_item {
                        inner.remember_sent(header.frame_id);
                        self.hold(&inner, &it.data, false);
                    }
                    // A run from before it is over; one after it (the anchor still queued from
                    // an earlier flush) stands.
                    if let Some(first) = inner.lost_first {
                        let diff = header.frame_id.wrapping_sub(first);
                        if diff > 0 && diff < 32768 {
                            inner.end_lost();
                        }
                    }
                    kept.push(it);
                    continue;
                }
                inner.forget_sent(header.frame_id);
                inner.lose(header.frame_id, at);
            }
            // The chunk held back counts whether or not one was handed in, as the backlog does.
            let missed = (1 + backlog_len).saturating_sub(kept.len()) as u64;
            self.judge(&mut inner, missed);
            inner.backlog_bytes = kept.iter().map(|i| i.data.len()).sum();
            inner.backlog.extend(kept);
            if !inner.backlog.is_empty() {
                self.wake.notify_one();
            }
            return;
        }
        if counted {
            self.judge(&mut inner, 1 + backlog_len as u64);
        }
        inner.gated = false;
        if !items.is_empty() || !inner.live_rows.is_empty() {
            inner.backlog_bytes = 0;
            inner.live_rows.clear();
            drop(inner);
            self.leave_common();
        }
    }

    /// The gate lifted: whether this client, whose acks caught up (`resync`), resumes on its
    /// own, on the frame predicting past the run its gate opened, or on the next where none
    /// was dropped. Otherwise every row waits for the IDR the lift asks for.
    pub fn ungate(&self, resync: bool) -> bool {
        let mut inner = self.inner.lock();
        let gated = std::mem::replace(&mut inner.gated, false);
        if resync && inner.tracked && !inner.live_rows.is_empty() {
            if inner.lost_first.is_some() {
                inner.lost_since = now();
                inner.lost_gated = true;
            }
            return true;
        }
        if gated {
            inner.live_rows.clear();
            drop(inner);
            self.leave_common();
        }
        false
    }

    /// A key frame for every row is already asked for (the gate's lift): the deltas dropped
    /// until it arrives ask for no other within the sync floor.
    pub fn hold_sync(&self) {
        self.inner.lock().next_sync_req = now() + VIDEO_RELAY_SYNC_FLOOR_SECONDS;
    }

    fn want_sync(inner: &mut Inner) -> bool {
        let at = now();
        if at >= inner.next_sync_req {
            inner.next_sync_req = at + VIDEO_RELAY_SYNC_FLOOR_SECONDS;
            return true;
        }
        false
    }

    /// This client's frames still on their way out of this host: those queued here, and those
    /// handed to its socket whose last byte it has not sent.
    fn frames_ahead(&self, inner: &mut Inner) -> usize {
        if !inner.marks.is_empty() {
            match self.client.write_backlog() {
                None => inner.marks.clear(),
                Some(unsent) => {
                    while let Some(mark) = inner.marks.front() {
                        if inner.written.saturating_sub(*mark) as i64 >= unsent {
                            inner.marks.pop_front();
                        } else {
                            break;
                        }
                    }
                }
            }
        }
        inner.backlog.len() + inner.marks.len()
    }

    /// How long the oldest ping written behind this client's frames has gone unanswered past
    /// the path's round trip: the queue ahead of it on the path.
    fn lag(&self, inner: &mut Inner) -> f64 {
        let at = now();
        let mut uplink = self.client.uplink.lock();
        if uplink.seq != inner.pongs
            && let Some(rtt_us) = uplink.rtt_us
        {
            inner.pongs = uplink.seq;
            let rtt = rtt_us as f64 / 1e6;
            let floor = uplink.observe_floor(rtt_us, at) as f64 / 1e6;
            inner.rtt_floor = Some(floor);
            self.settle_floor(inner, rtt, floor, at);
        }
        if let (Some(settled), Some(floor)) = (inner.settled_floor, inner.rtt_floor) {
            inner.rtt_floor = Some(floor.max(settled));
        }
        let Some(floor) = inner.rtt_floor else {
            return 0.0;
        };
        match uplink.oldest_unanswered() {
            None => 0.0,
            Some(oldest) => (at - oldest - floor).max(0.0),
        }
    }

    /// Fold a pong's round trip `rtt` into the settle span over the clock's `floor`, moving the
    /// settled floor onto a least round trip that stood past it through the span, or down to a
    /// lower round trip.
    fn settle_floor(&self, inner: &mut Inner, rtt: f64, floor: f64, at: f64) {
        let recent = &mut inner.recent_rtts;
        recent.push_back((at, rtt));
        while recent
            .front()
            .is_some_and(|(t, _)| at - t > VIDEO_RELAY_SETTLE_SECONDS)
        {
            recent.pop_front();
        }
        if inner.settled_floor.is_some_and(|settled| rtt < settled) {
            inner.settled_floor = Some(rtt.max(floor));
        }
        let least = recent.iter().map(|(_, r)| *r).fold(f64::INFINITY, f64::min);
        let current = floor.max(inner.settled_floor.unwrap_or(0.0));
        let span = recent.front().map_or(0.0, |(t, _)| at - t);
        if span >= VIDEO_RELAY_SETTLE_SECONDS * 0.9 && least - current >= VIDEO_RELAY_LAG_SECONDS {
            info!(target: "ws", "Video relay[{}] for {}: round trip floor {:.0} -> {:.0} ms (a queue on its path not its own)",
                self.display_id, self.client.remote, current * 1000.0, least * 1000.0);
            inner.settled_floor = Some(least);
        }
    }

    fn room(&self, inner: &mut Inner) -> bool {
        self.frames_ahead(inner) < VIDEO_RELAY_ROOM_FRAMES
            && self.lag(inner) < VIDEO_RELAY_LAG_SECONDS
    }

    fn anchor_room(&self, inner: &mut Inner) -> bool {
        !self.far(inner) && self.lag(inner) < VIDEO_RELAY_ANCHOR_SECONDS
    }

    fn far(&self, inner: &mut Inner) -> bool {
        self.frames_ahead(inner) >= VIDEO_RELAY_FAR_FRAMES
            || self.lag(inner) >= VIDEO_RELAY_FAR_SECONDS
    }

    /// Whether a delta frame naming the frame it predicts from goes to this client; `None`
    /// where only a key frame brings the client back.
    fn forward(
        &self,
        inner: &mut Inner,
        frame_id: u16,
        reference: u16,
        anchor: bool,
    ) -> Option<bool> {
        let at = now();
        let kept = inner.numbered && inner.since_key.is_multiple_of(FRAME_NUM_WRAP);
        if !inner.sent_set.contains(&reference) {
            inner.lose(frame_id, at);
            return self.repair(inner);
        }
        if inner.lost_first.is_some() {
            if !anchor || kept || self.anchor_room(inner) {
                inner.end_lost();
                return Some(true);
            }
            inner.lost_first = Some(frame_id);
            inner.lost_told = false;
            inner.lose(frame_id, at);
            return self.repair(inner);
        }
        if self.host.owns_display(self.client.id, &self.display_id) {
            return Some(true);
        }
        inner.gauged = true;
        if anchor && self.anchor_room(inner) {
            return Some(true);
        }
        if !kept && !self.room(inner) {
            inner.lose(frame_id, at);
            return self.repair(inner);
        }
        Some(true)
    }

    /// A frame of the open run was held back: tell the encoder of the run's first frame once
    /// the client has room for the frame predicting past it, or sooner where waiting would
    /// cost a key frame. `Some(false)` while the run waits; `None` once it has waited out its
    /// recovery window.
    fn repair(&self, inner: &mut Inner) -> Option<bool> {
        let limit = if inner.anchored && !inner.lost_gated {
            VIDEO_RELAY_ANCHORED_RECOVERY_SECONDS
        } else {
            VIDEO_RELAY_LOST_RECOVERY_SECONDS
        };
        if now() - inner.lost_since > limit {
            return None;
        }
        if !inner.lost_told {
            let wrap_lead = (FRAME_NUM_WRAP - inner.since_key % FRAME_NUM_WRAP) % FRAME_NUM_WRAP;
            let due = inner.lost_run >= VIDEO_RELAY_REACH_FRAMES
                || (inner.numbered && wrap_lead <= VIDEO_RELAY_WRAP_LEAD);
            if (self.room(inner) || (due && !self.far(inner)))
                && let Some(first) = inner.lost_first
            {
                inner.lost_told = self.host.relay_lost(
                    self.client.id,
                    &self.display_id,
                    first,
                    inner.lost_run <= VIDEO_RELAY_REACH_FRAMES,
                );
            }
        }
        Some(false)
    }

    /// Accept, drop, or gate one encoded chunk; whether the caller should request a key frame
    /// (data was dropped that only a sync point recovers, within the sync floor). Never awaits.
    pub fn offer(&self, item: RelayItem) -> bool {
        let mut inner = self.inner.lock();
        let size = item.data.len();
        let header = video_header(&item.data);
        let is_idr = header.is_some_and(|h| h.key);
        let mut dropped = false;
        let mut flushed = 0u64;
        if !is_idr && !inner.backlog.is_empty() && inner.backlog_bytes + size > self.budget {
            flushed = inner.backlog.len() as u64;
            inner.backlog.clear();
            inner.backlog_bytes = 0;
            inner.live_rows.clear();
            inner.end_lost();
            self.leave_common();
            dropped = true;
        }
        let mut deliver = true;
        if let Some(h) = header {
            if !h.key {
                inner.tracked = h.reference != h.frame_id;
            }
            if h.key {
                inner.since_key = 0;
                inner.numbered = h.codec == WIRE_H264;
                inner.anchored = false;
                inner.live_rows.insert(h.row);
                inner.end_lost();
                inner.clear_sent();
                self.join_common();
            } else if !inner.live_rows.contains(&h.row) {
                deliver = false;
                dropped = true;
            } else if h.reference != h.frame_id {
                inner.since_key += 1;
                inner.anchored = inner.anchored || h.anchor;
                let forward = self.forward(&mut inner, h.frame_id, h.reference, h.anchor);
                deliver = forward == Some(true);
                if forward.is_none() {
                    inner.live_rows.clear();
                    inner.end_lost();
                    self.leave_common();
                    dropped = true;
                }
            }
        }
        if deliver {
            if let Some(h) = header {
                inner.remember_sent(h.frame_id);
            }
            inner.backlog_bytes += size;
            if header.is_some() {
                self.hold(&inner, &item.data, false);
            }
            inner.backlog.push_back(item);
            self.wake.notify_one();
        }
        self.judge(&mut inner, flushed + u64::from(!deliver));
        dropped && Self::want_sync(&mut inner)
    }

    /// The fan-out itself fell behind the capture and lost frames for every relay: every row
    /// waits for the key frame the fan-out asks for.
    pub fn drop_for_lag(&self) {
        {
            let mut inner = self.inner.lock();
            inner.live_rows.clear();
            inner.end_lost();
        }
        self.leave_common();
    }

    /// Pad this client's stream at `bps` until `until` (`PathProbe`), in chunks written while
    /// no frame waits, or stop padding where `bps` is 0.
    pub fn set_padding(&self, bps: f64, until: f64) {
        let start = {
            let mut inner = self.inner.lock();
            let at = now();
            let start = bps > 0.0 && (inner.pad_bps <= 0.0 || at >= inner.pad_until);
            inner.pad_bps = bps.max(0.0);
            inner.pad_until = until;
            if start {
                inner.pad_next = at;
            }
            start
        };
        if start {
            self.wake.notify_one();
        }
    }

    /// Write one padding chunk of `size` bytes to the display's owner, counted in what the
    /// display sent, with a ping behind it each `PAD_PING_S` whose pong acks the bytes sent
    /// through it for the steer. Pinged only with no frame awaiting its ack: the browser answers
    /// a ping at once and the page acks a frame on its own tick, so a ping behind an unacked
    /// frame would ack bytes past it first and the acked history would run out of order.
    /// False where the socket died.
    async fn write_padding(&self, size: usize) -> bool {
        let Some((link, sent_bytes)) =
            self.host
                .note_padding(self.client.id, &self.display_id, size)
        else {
            return true;
        };
        let what = format!("Padding for '{}'", self.display_id);
        if self
            .client
            .send_live(Frame::Binary(PADDING.slice(..size)), &what)
            .await
            .is_err()
        {
            return false;
        }
        let at = now();
        let due = {
            let mut inner = self.inner.lock();
            let due = at - inner.pad_pinged >= PAD_PING_S;
            if due && link.lock().unacked_since.is_none() {
                inner.pad_pinged = at;
                true
            } else {
                false
            }
        };
        if due {
            let (host, client, display_id) =
                (self.host.clone(), self.client.id, self.display_id.clone());
            self.client.ping_behind_answered(Box::new(move |rtt_s| {
                let rtt_ms = rtt_s * 1000.0;
                if host.owns_display(client, &display_id)
                    && (0.0..=RTT_SAMPLE_SANE_MAX_MS).contains(&rtt_ms)
                {
                    link.lock().note_round_trip(rtt_ms, sent_bytes, now());
                }
            }));
        }
        true
    }

    /// Drain the backlog onto the socket until stopped or the socket dies, padding the stream
    /// (`set_padding`) while it holds no frame.
    async fn run(self: Arc<Self>) {
        loop {
            let (item, pad, wait) = {
                let mut inner = self.inner.lock();
                if inner.stopped {
                    break;
                }
                match inner.backlog.pop_front() {
                    Some(item) => {
                        inner.backlog_bytes -= item.data.len();
                        inner.written += item.data.len() as u64;
                        let written = inner.written;
                        inner.marks.push_back(written);
                        while inner.marks.len() > LOST_FRAME_MEMORY {
                            inner.marks.pop_front();
                        }
                        (Some(item), None, None)
                    }
                    None => {
                        let at = now();
                        let pad = inner.padding_due(at);
                        let padding = inner.pad_bps > 0.0 && at < inner.pad_until;
                        let wait = padding.then(|| (inner.pad_next - at).max(0.0));
                        (None, pad, wait)
                    }
                }
            };
            if let Some(size) = pad {
                if !self.write_padding(size).await {
                    self.host.socket_died(self.client.id);
                    break;
                }
                continue;
            }
            let Some(item) = item else {
                match wait {
                    Some(wait) => {
                        let _ = tokio::time::timeout(
                            Duration::from_secs_f64(wait),
                            self.wake.notified(),
                        )
                        .await;
                    }
                    None => self.wake.notified().await,
                }
                continue;
            };
            // Stamped before the await and only for the display's owner: that is what the
            // ACK round trip measures, and the hand to the socket is where the stats read a
            // frame as sent.
            let ping_behind = self.host.note_send(self.client.id, &self.display_id, &item);
            let what = format!("Video relay for '{}'", self.display_id);
            let data = item.data;
            match self
                .client
                .send_live_pinged(Frame::Binary(data.clone()), &what, ping_behind)
                .await
            {
                Ok(()) => {
                    let gauged = {
                        let inner = self.inner.lock();
                        if !inner.stopped && video_header(&data).is_some() {
                            self.hold(&inner, &data, true);
                        }
                        inner.gauged
                    };
                    if gauged {
                        let at = now();
                        let mut inner = self.inner.lock();
                        if at - inner.pinged >= VIDEO_RELAY_PING_SECONDS {
                            inner.pinged = at;
                            drop(inner);
                            self.client.ping_behind();
                        }
                    }
                }
                Err(_) => {
                    self.host.socket_died(self.client.id);
                    break;
                }
            }
        }
        self.host.relay_finished(&self.display_id, &self);
    }

    // Test hooks over the relay's state.

    /// The frame ids queued, oldest first.
    pub fn backlog_ids(&self) -> Vec<u16> {
        self.inner
            .lock()
            .backlog
            .iter()
            .map(|i| i.frame_id)
            .collect()
    }

    pub fn backlog_len(&self) -> usize {
        self.inner.lock().backlog.len()
    }

    pub fn lost_first(&self) -> Option<u16> {
        self.inner.lock().lost_first
    }

    pub fn has_live_rows(&self) -> bool {
        !self.inner.lock().live_rows.is_empty()
    }

    pub fn was_sent(&self, frame_id: u16) -> bool {
        self.inner.lock().sent_set.contains(&frame_id)
    }

    /// The queue ahead of the oldest unanswered ping, as `forward` reads it.
    pub fn current_lag(&self) -> f64 {
        let mut inner = self.inner.lock();
        self.lag(&mut inner)
    }

    /// Test hook: the round trip floor the lag is read from, and the settled one in it.
    #[cfg(test)]
    pub(crate) fn floors(&self) -> (Option<f64>, Option<f64>) {
        let inner = self.inner.lock();
        (inner.rtt_floor, inner.settled_floor)
    }

    /// Test hook: the drain task hands the oldest `count` queued frames to the socket, which
    /// sends none yet; `unsent` grows by their bytes.
    #[cfg(test)]
    pub(crate) fn hand(&self, count: usize, unsent: &std::sync::atomic::AtomicI64) {
        let mut inner = self.inner.lock();
        for _ in 0..count {
            let Some(item) = inner.backlog.pop_front() else {
                break;
            };
            let n = item.data.len();
            inner.backlog_bytes -= n;
            inner.written += n as u64;
            let written = inner.written;
            inner.marks.push_back(written);
            unsent.fetch_add(n as i64, std::sync::atomic::Ordering::Relaxed);
            self.hold(&inner, &item.data, true);
        }
    }

    /// Test hook: `count` frames of a thousand bytes each handed to the socket and not sent.
    #[cfg(test)]
    pub(crate) fn full(&self, count: usize, unsent: &std::sync::atomic::AtomicI64) {
        let mut inner = self.inner.lock();
        for _ in 0..count {
            inner.written += 1000;
            let written = inner.written;
            inner.marks.push_back(written);
        }
        unsent.fetch_add(1000 * count as i64, std::sync::atomic::Ordering::Relaxed);
    }

    /// Test hook: marks standing for frames far behind.
    #[cfg(test)]
    pub(crate) fn mark_far(&self, count: usize) {
        let mut inner = self.inner.lock();
        for i in 0..count {
            inner.marks.push_back(i as u64);
        }
    }

    /// Test hook: empty the backlog as a drain that sent at once would.
    #[cfg(test)]
    pub(crate) fn clear_backlog(&self) {
        let mut inner = self.inner.lock();
        inner.backlog.clear();
        inner.backlog_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::client::Liveness;
    use crate::ws::client::testing::{FakeSocket, Page};
    use crate::ws::clock_tests::{advance, reset_clock};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::time::Duration;

    const ROOM: usize = VIDEO_RELAY_ROOM_FRAMES;
    const REACH: u32 = VIDEO_RELAY_REACH_FRAMES;
    const FAR: usize = VIDEO_RELAY_FAR_FRAMES;
    const WRAP: u32 = FRAME_NUM_WRAP;

    /// A `0x04` chunk with the wire header the relay reads.
    fn chunk(
        frame_id: u16,
        reference: u16,
        key: bool,
        size: usize,
        row: u16,
        codec: u8,
        anchor: bool,
    ) -> RelayItem {
        let kind = (if key { 0x01u8 } else { 0x00 }) | if anchor { FRAME_ANCHOR } else { 0 };
        let mut data = vec![OPCODE_VIDEO, (codec << 4) | kind];
        data.extend_from_slice(&frame_id.to_be_bytes());
        data.extend_from_slice(&row.to_be_bytes());
        data.extend_from_slice(&[0x07, 0x80, 0x04, 0x38]);
        data.extend_from_slice(&reference.to_be_bytes());
        data.extend(std::iter::repeat_n(0u8, size));
        RelayItem {
            data: Bytes::from(data),
            frame_id,
            capture_ns: 0,
            encode_start_ns: 0,
            encode_end_ns: 0,
        }
    }

    fn delta(frame_id: u16, reference: u16) -> RelayItem {
        chunk(frame_id, reference, false, 400, 0, WIRE_H264, false)
    }

    fn key(frame_id: u16) -> RelayItem {
        chunk(frame_id, frame_id, true, 400, 0, WIRE_H264, false)
    }

    fn anchor(frame_id: u16, reference: u16) -> RelayItem {
        chunk(frame_id, reference, false, 400, 0, WIRE_H264, true)
    }

    /// A server recording the lost reports as (frame, within reach) and the frames every
    /// relay's page holds (`acked`) and was handed (`handed`).
    struct Server {
        owner: Mutex<Option<ClientId>>,
        taken: AtomicBool,
        reports: Mutex<Vec<(u16, bool)>>,
        acked: Arc<Mutex<Vec<u16>>>,
        handed: Arc<Mutex<Vec<u16>>>,
        common: Mutex<HashMap<(String, bool), CommonFrames<ClientId>>>,
        verdicts: Mutex<Vec<(ClientId, bool)>>,
        died: Mutex<Vec<ClientId>>,
    }

    impl Server {
        fn new(taken: bool) -> Arc<Server> {
            Arc::new(Server {
                owner: Mutex::new(None),
                taken: AtomicBool::new(taken),
                reports: Mutex::new(Vec::new()),
                acked: Arc::new(Mutex::new(Vec::new())),
                handed: Arc::new(Mutex::new(Vec::new())),
                common: Mutex::new(HashMap::new()),
                verdicts: Mutex::new(Vec::new()),
                died: Mutex::new(Vec::new()),
            })
        }

        fn reports(&self) -> Vec<(u16, bool)> {
            self.reports.lock().clone()
        }
    }

    impl RelayHost for Server {
        fn owns_display(&self, client: ClientId, _display_id: &str) -> bool {
            *self.owner.lock() == Some(client)
        }

        fn relay_lost(
            &self,
            _client: ClientId,
            _display_id: &str,
            frame_id: u16,
            reach: bool,
        ) -> bool {
            if !reach && !self.taken.load(Ordering::Relaxed) {
                return false;
            }
            self.reports.lock().push((frame_id, reach));
            true
        }

        fn with_common_frames(
            &self,
            display_id: &str,
            held: bool,
            f: &mut dyn FnMut(&mut CommonFrames<ClientId>),
        ) {
            let told = if held {
                self.acked.clone()
            } else {
                self.handed.clone()
            };
            let mut common = self.common.lock();
            let entry = common
                .entry((display_id.to_string(), held))
                .or_insert_with(|| CommonFrames::new(move |frame| told.lock().push(frame)));
            f(entry);
        }

        fn note_send(&self, _client: ClientId, _display_id: &str, _item: &RelayItem) -> bool {
            false
        }

        fn connection_verdict(&self, client: ClientId, poor: bool) {
            self.verdicts.lock().push((client, poor));
        }

        fn socket_died(&self, client: ClientId) {
            self.died.lock().push(client);
        }

        fn relay_finished(&self, _display_id: &str, _relay: &Arc<VideoRelay>) {}

        fn note_padding(
            &self,
            _client: ClientId,
            _display_id: &str,
            _size: usize,
        ) -> Option<(Arc<Mutex<DisplayLink>>, u64)> {
            None
        }
    }

    struct Rig {
        server: Arc<Server>,
        page: Page,
        relay: Arc<VideoRelay>,
    }

    impl Rig {
        fn unsent(&self) -> &AtomicI64 {
            &self.page.unsent
        }

        fn hand(&self, n: usize) {
            self.relay.hand(n, self.unsent());
        }

        fn drained(&self) {
            self.unsent().store(0, Ordering::Relaxed);
        }

        fn full(&self) {
            self.relay.full(3, self.unsent());
        }

        fn ids(&self) -> Vec<u16> {
            self.relay.backlog_ids()
        }
    }

    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(100);

    fn rig(owner: bool, taken: bool, budget: usize, server: Option<Arc<Server>>) -> Rig {
        let server = server.unwrap_or_else(|| Server::new(taken));
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let page = Page::new(id, "controller");
        if owner {
            *server.owner.lock() = Some(id);
        }
        let relay = VideoRelay::new(server.clone(), "primary", page.client.clone(), budget);
        Rig {
            server,
            page,
            relay,
        }
    }

    #[tokio::test]
    async fn a_page_on_a_narrow_link_is_thinned_and_resynced_without_a_key_frame() {
        reset_clock();
        let r = rig(false, true, 100_000, None);
        let mut asks = vec![r.relay.offer(key(1))];
        r.hand(1);
        asks.extend([2, 3].map(|n| r.relay.offer(delta(n, n - 1))));
        assert_eq!(r.ids(), vec![2, 3], "frames go out while the page has room");
        asks.extend([4, 5].map(|n| r.relay.offer(delta(n, n - 1))));
        assert_eq!(
            (r.ids(), r.relay.lost_first()),
            (vec![2, 3], Some(4)),
            "with {ROOM} on their way the next is left out"
        );
        assert!(
            r.server.reports().is_empty(),
            "the encoder is not told while the page has no room"
        );
        r.hand(2);
        r.drained();
        asks.push(r.relay.offer(delta(6, 5)));
        assert_eq!(
            r.server.reports(),
            vec![(4, true)],
            "once the frames ahead left, the run's first frame is reported"
        );
        asks.push(r.relay.offer(delta(7, 6)));
        assert_eq!((r.ids(), r.server.reports()), (vec![], vec![(4, true)]));
        asks.push(r.relay.offer(delta(8, 3)));
        assert_eq!(
            r.ids(),
            vec![8],
            "the frame the encoder predicted from the last one sent goes out"
        );
        asks.push(r.relay.offer(delta(9, 8)));
        assert_eq!(r.ids(), vec![8, 9]);
        assert!(!asks.iter().any(|a| *a), "without a key frame asked for");
    }

    #[tokio::test]
    async fn a_run_at_the_reach_is_reported_and_answered_without_room() {
        reset_clock();
        let r = rig(false, true, 100_000, None);
        r.relay.offer(key(1));
        r.hand(1);
        for n in 2..(2 + ROOM as u16 - 1) {
            r.relay.offer(delta(n, n - 1));
        }
        let first = 2 + ROOM as u16 - 1;
        for n in first..(first + REACH as u16 - 1) {
            r.relay.offer(delta(n, n - 1));
        }
        assert!(
            r.server.reports().is_empty(),
            "a run shorter than the reach waits for room"
        );
        r.relay
            .offer(delta(first + REACH as u16 - 1, first + REACH as u16 - 2));
        assert_eq!(r.server.reports(), vec![(first, true)]);
        let queued = r.ids();
        r.relay.offer(delta(first + REACH as u16, first - 1));
        let mut expect = queued;
        expect.push(first + REACH as u16);
        assert_eq!(
            r.ids(),
            expect,
            "the frame answering it goes out though the page has no room"
        );
    }

    #[tokio::test]
    async fn a_page_far_behind_waits_for_its_repair_to_be_taken() {
        reset_clock();
        let r = rig(false, false, 100_000, None);
        for n in 1..(1 + FAR as u16) {
            r.relay.offer(key(n));
        }
        let queued = r.relay.backlog_len();
        r.hand(queued);
        let first = 1 + FAR as u16;
        for n in first..(first + REACH as u16 + 2) {
            r.relay.offer(delta(n, n - 1));
        }
        assert!(
            r.server.reports().is_empty(),
            "a page far behind is not answered at the reach"
        );
        r.drained();
        r.relay
            .offer(delta(first + REACH as u16 + 2, first + REACH as u16 + 1));
        assert!(
            r.server.reports().is_empty(),
            "a report past the reach waits while the key-frame requests are spent"
        );
        r.server.taken.store(true, Ordering::Relaxed);
        r.relay
            .offer(delta(first + REACH as u16 + 3, first + REACH as u16 + 2));
        assert_eq!(r.server.reports(), vec![(first, false)]);
    }

    #[tokio::test]
    async fn a_frame_num_wrap_is_never_left_out() {
        reset_clock();
        let r = rig(false, true, 100_000, None);
        r.relay.offer(key(1));
        for n in 2..(WRAP as u16 - 1) {
            r.hand(1);
            r.drained();
            r.relay.offer(delta(n, n - 1));
        }
        r.hand(1);
        r.full();
        let n = WRAP as u16 - 1;
        r.relay.offer(delta(n, n - 1));
        assert_eq!(
            (r.relay.lost_first(), r.server.reports()),
            (Some(n), vec![(n, true)]),
            "reported at once, room or not"
        );
        r.relay.offer(delta(n + 1, n - 1));
        r.relay.offer(delta(n + 2, n + 1));
        let ids = r.ids();
        assert_eq!(
            (&ids[ids.len() - 2..], r.relay.lost_first()),
            (&[n + 1, n + 2][..], None)
        );
        r.relay.offer(delta(n + 3, n + 2));
        assert_eq!(
            r.relay.lost_first(),
            Some(n + 3),
            "past it a page without room is left frames again"
        );

        let r = rig(false, true, 100_000, None);
        let av1 = 4u8;
        r.relay.offer(chunk(1, 1, true, 400, 0, av1, false));
        for n in 2..=(WRAP as u16) {
            r.hand(1);
            r.drained();
            r.relay.offer(chunk(n, n - 1, false, 400, 0, av1, false));
        }
        r.hand(1);
        r.full();
        r.relay.offer(chunk(
            WRAP as u16 + 1,
            WRAP as u16,
            false,
            400,
            0,
            av1,
            false,
        ));
        assert_eq!(
            (r.relay.lost_first(), r.server.reports()),
            (Some(WRAP as u16 + 1), vec![]),
            "a codec without frame_num is not reported early"
        );
    }

    #[tokio::test]
    async fn arrival_is_read_end_to_end_from_the_pings() {
        reset_clock();
        let r = rig(false, true, 100_000, None);
        let t = now();
        r.page.client.uplink.lock().seed(1, 20_000, t - 1.0);
        r.relay.offer(key(1));
        r.relay.offer(delta(2, 1));
        r.hand(2);
        r.drained();
        let behind = r.page.client.uplink.lock().pending_since(t - 0.3);
        r.relay.offer(delta(3, 2));
        assert_eq!(
            r.relay.lost_first(),
            Some(3),
            "a ping unanswered past the round trip leaves the page no room"
        );
        {
            let mut up = r.page.client.uplink.lock();
            up.forget(behind);
            up.seed(2, 20_000, t - 0.3);
        }
        r.relay.offer(delta(4, 3));
        assert_eq!(
            r.server.reports(),
            vec![(3, true)],
            "its pong gives the room back, and the run is reported"
        );
        {
            let mut up = r.page.client.uplink.lock();
            up.pending_since(t - 5.0);
            up.answered = t - 0.1;
        }
        assert_eq!(
            r.relay.current_lag(),
            0.0,
            "a ping older than the newest answered was lost"
        );
    }

    /// A pong every `step` for `seconds` at `rtt_ms`, each read by the relay's lag.
    fn pongs(r: &Rig, rtt_ms: f64, seconds: f64) {
        let step = 0.25;
        let mut left = seconds;
        while left > 1e-9 {
            advance(step);
            left -= step;
            {
                let mut up = r.page.client.uplink.lock();
                let (seq, answered) = (up.seq + 1, up.answered);
                up.seed(seq, (rtt_ms * 1000.0) as i64, answered);
            }
            r.relay.current_lag();
        }
    }

    #[tokio::test]
    async fn a_queue_other_traffic_keeps_standing_becomes_the_floor() {
        reset_clock();
        let close = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() < 1e-6);
        let r = rig(false, true, 100_000, None);
        pongs(&r, 145.0, 4.0);
        assert!(
            close(r.relay.floors().0, 0.145),
            "the floor is the least round trip"
        );
        pongs(&r, 295.0, VIDEO_RELAY_SETTLE_SECONDS * 0.6);
        let (floor, settled) = r.relay.floors();
        assert!(
            settled.is_none() && close(floor, 0.145),
            "a standing queue held under the settle span stays a queue: {floor:?} {settled:?}"
        );
        pongs(&r, 295.0, VIDEO_RELAY_SETTLE_SECONDS);
        let (floor, settled) = r.relay.floors();
        assert!(
            settled.is_some() && close(floor, 0.295),
            "held through it, it becomes the floor"
        );
        let behind = {
            let mut up = r.page.client.uplink.lock();
            up.answered = now() - 1.0;
            up.pending_since(now() - 0.35)
        };
        assert!(
            r.relay.current_lag() < VIDEO_RELAY_LAG_SECONDS,
            "a ping unanswered as long as that path takes is no lag"
        );
        r.page.client.uplink.lock().forget(behind);
        pongs(&r, 150.0, 1.0);
        assert!(
            close(r.relay.floors().0, 0.150),
            "a lower round trip takes the floor back down"
        );

        let r = rig(false, true, 100_000, None);
        pongs(&r, 145.0, 4.0);
        for _ in 0..4 {
            pongs(&r, 400.0, 0.75);
            pongs(&r, 170.0, 0.75);
        }
        let (floor, settled) = r.relay.floors();
        assert!(
            settled.is_none() && close(floor, 0.145),
            "a queue drained within the span never settles: {floor:?} {settled:?}"
        );
    }

    #[tokio::test]
    async fn a_run_never_predicted_past_falls_back_to_a_key_frame() {
        reset_clock();
        let r = rig(false, true, 100_000, None);
        r.relay.offer(key(1));
        r.hand(1);
        for n in 2..(2 + ROOM as u16 + 2) {
            r.relay.offer(delta(n, n - 1));
        }
        advance(VIDEO_RELAY_LOST_RECOVERY_SECONDS + 0.1);
        assert!(
            r.relay
                .offer(delta(2 + ROOM as u16 + 2, 2 + ROOM as u16 + 1)),
            "after the recovery window a held frame asks for a key frame"
        );
        r.relay.offer(delta(20, 1));
        assert_ne!(r.ids().last(), Some(&20), "the client waits for it, gated");
        r.relay.offer(key(21));
        assert_eq!(r.ids().last(), Some(&21));
    }

    #[tokio::test]
    async fn the_owner_is_never_thinned() {
        reset_clock();
        let r = rig(true, true, 100_000, None);
        r.relay.offer(key(1));
        r.hand(1);
        for n in 2..12 {
            r.relay.offer(delta(n, n - 1));
        }
        assert_eq!(r.ids(), (2..12).collect::<Vec<u16>>());
        assert!(r.server.reports().is_empty());
    }

    #[tokio::test]
    async fn the_encoder_is_told_of_each_frame_every_page_holds() {
        reset_clock();
        let o = rig(true, true, 100_000, None);
        let r = rig(false, true, 100_000, Some(o.server.clone()));
        for q in [&o, &r] {
            q.relay.offer(key(1));
        }
        assert_eq!(
            (
                o.server.handed.lock().clone(),
                o.server.acked.lock().clone()
            ),
            (vec![1], vec![])
        );
        o.hand(1);
        assert!(
            o.server.acked.lock().is_empty(),
            "a key frame one page holds is not yet common"
        );
        r.hand(1);
        assert_eq!(o.server.acked.lock().clone(), vec![1]);
        for n in 2..=3u16 {
            for q in [&o, &r] {
                q.relay.offer(delta(n, n - 1));
                q.hand(1);
                q.drained();
            }
        }
        assert_eq!(o.server.acked.lock().clone(), vec![1, 2, 3]);
        r.full();
        for n in 4..=6u16 {
            o.relay.offer(delta(n, n - 1));
            o.hand(1);
            r.relay.offer(delta(n, n - 1));
        }
        assert_eq!(
            (r.relay.lost_first(), o.server.acked.lock().clone()),
            (Some(4), vec![1, 2, 3])
        );
        assert_eq!(o.server.handed.lock().clone(), vec![1, 2, 3]);
        o.relay.offer(anchor(12, 3));
        o.hand(1);
        r.relay.offer(anchor(12, 3));
        assert_eq!(
            (r.ids().last(), r.relay.lost_first()),
            (Some(&12), None),
            "an anchor predicting from a common frame ends the run"
        );
        assert_eq!(
            (o.server.handed.lock().last(), o.server.acked.lock().last()),
            (Some(&12), Some(&3))
        );
        r.hand(1);
        assert_eq!(o.server.acked.lock().last(), Some(&12));
        for n in 13..(13 + REACH as u16 - 1) {
            o.relay.offer(delta(n, n - 1));
            r.relay.offer(delta(n, n - 1));
        }
        assert_eq!(
            (r.relay.lost_first(), r.server.reports()),
            (Some(13), vec![])
        );
        r.relay.mark_far(FAR);
        o.relay.offer(anchor(24, 12));
        r.relay.offer(anchor(24, 12));
        assert!(
            r.relay.lost_first() == Some(24) && r.ids().last() != Some(&24),
            "a page too far behind is not sent an anchor"
        );
        r.relay.offer(delta(26, 4));
        assert!(
            r.ids().last() != Some(&26) && r.relay.lost_first() == Some(24),
            "a frame predicting from one never sent is held back"
        );
        r.relay.flush_for_gate(false, None);
        o.relay.offer(delta(25, 24));
        let n = o.relay.backlog_len();
        o.hand(n);
        assert_eq!(
            o.server.acked.lock().last(),
            Some(&25),
            "a page waiting for a key frame holds no frame back"
        );
        r.relay.stop();
        o.relay.stop();
    }

    #[tokio::test]
    async fn the_frame_after_a_key_frame_is_left_out_like_any_other() {
        reset_clock();
        let r = rig(false, true, 100_000, None);
        r.relay.offer(key(1));
        r.full();
        r.relay.offer(delta(2, 1));
        assert_eq!((r.ids().last(), r.relay.lost_first()), (Some(&1), Some(2)));
    }

    #[tokio::test]
    async fn an_anchored_run_waits_longer_for_room() {
        reset_clock();
        let r = rig(false, true, 100_000, None);
        r.relay.offer(key(1));
        r.relay.offer(delta(2, 1));
        r.hand(2);
        r.drained();
        r.relay.offer(anchor(12, 2));
        r.hand(1);
        r.full();
        r.relay.mark_far(FAR);
        for n in 13..16 {
            r.relay.offer(delta(n, n - 1));
        }
        advance(VIDEO_RELAY_LOST_RECOVERY_SECONDS + 0.5);
        let asked = r.relay.offer(delta(16, 15));
        assert!(
            !asked && r.relay.lost_first() == Some(13),
            "an anchored run is not answered with a key frame after a second"
        );
        advance(VIDEO_RELAY_ANCHORED_RECOVERY_SECONDS);
        assert!(r.relay.offer(delta(17, 16)), "but is past its own limit");
    }

    #[tokio::test]
    async fn the_owners_gate_opens_a_run_and_lifts_without_a_key_frame() {
        reset_clock();
        let r = rig(true, true, 100_000, None);
        r.relay.offer(key(1));
        for n in 2..=3 {
            r.relay.offer(delta(n, n - 1));
        }
        r.hand(3);
        r.drained();
        for n in 4..=5 {
            r.relay.offer(delta(n, n - 1));
        }
        r.relay.flush_for_gate(true, Some(delta(6, 5)));
        for n in 7..=8 {
            r.relay.flush_for_gate(true, Some(delta(n, n - 1)));
        }
        assert!(
            r.ids().is_empty()
                && r.relay.lost_first() == Some(4)
                && r.relay.has_live_rows()
                && !r.relay.was_sent(4)
        );
        assert!(
            r.server.reports().is_empty(),
            "tells the encoder nothing while it holds"
        );
        assert!(
            r.relay.ungate(true),
            "the lift, the acks caught up, needs no key frame"
        );
        let asked = r.relay.offer(delta(9, 8));
        assert!(r.server.reports() == vec![(4, false)] && !asked && r.ids().is_empty());
        let asked = r.relay.offer(anchor(10, 1));
        assert!(!asked && r.ids() == vec![10] && r.relay.lost_first().is_none());

        let r = rig(true, true, 100_000, None);
        r.relay.offer(key(1));
        r.relay.offer(anchor(2, 1));
        r.hand(2);
        r.drained();
        r.relay.flush_for_gate(true, Some(delta(3, 2)));
        advance(VIDEO_RELAY_ANCHORED_RECOVERY_SECONDS + 1.0);
        assert!(r.relay.ungate(true) && !r.relay.offer(delta(4, 3)));
        assert_eq!(
            r.server.reports().iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![3]
        );
        advance(VIDEO_RELAY_LOST_RECOVERY_SECONDS / 2.0);
        assert!(
            !r.relay.offer(delta(5, 4)) && r.ids().is_empty(),
            "waits for the frame predicting past it"
        );
        advance(VIDEO_RELAY_LOST_RECOVERY_SECONDS);
        assert!(
            r.relay.offer(delta(6, 5)),
            "no longer than the unanchored wait"
        );

        let r = rig(true, true, 100_000, None);
        r.relay.offer(key(1));
        r.relay.offer(delta(2, 1));
        r.hand(2);
        r.relay.flush_for_gate(true, None);
        assert!(
            r.relay.ungate(true) && !r.relay.offer(delta(3, 2)) && r.ids() == vec![3],
            "a gate on which nothing was dropped resumes as it stood"
        );
    }

    #[tokio::test]
    async fn an_anchor_the_client_can_decode_passes_the_gate() {
        reset_clock();
        let r = rig(true, true, 100_000, None);
        r.relay.offer(key(1));
        for n in 2..=3 {
            r.relay.offer(delta(n, n - 1));
        }
        r.hand(3);
        r.drained();
        r.relay.flush_for_gate(true, Some(delta(4, 3)));
        r.relay.flush_for_gate(true, Some(delta(5, 4)));
        r.relay.flush_for_gate(true, Some(anchor(6, 3)));
        assert!(r.ids() == vec![6] && r.relay.was_sent(6) && r.relay.lost_first().is_none());
        for n in 7..=8 {
            r.relay.flush_for_gate(true, Some(delta(n, n - 1)));
        }
        assert!(
            r.ids() == vec![6] && r.relay.lost_first() == Some(7) && r.server.reports().is_empty()
        );
        assert!(r.relay.ungate(true));
        let asked = r.relay.offer(delta(9, 8));
        assert!(r.server.reports().iter().map(|r| r.0).collect::<Vec<_>>() == vec![7] && !asked);
        let asked = r.relay.offer(delta(10, 6));
        assert!(!asked && r.ids() == vec![6, 10] && r.relay.lost_first().is_none());

        let r = rig(true, true, 100_000, None);
        r.relay.offer(key(1));
        r.relay.offer(delta(2, 1));
        r.hand(2);
        r.drained();
        r.relay.flush_for_gate(true, Some(delta(3, 2)));
        r.relay.flush_for_gate(true, Some(anchor(4, 3)));
        assert!(
            r.ids().is_empty() && r.relay.lost_first() == Some(3),
            "an anchor predicting from a dropped frame is dropped"
        );

        let r = rig(true, true, 100_000, None);
        r.relay.offer(key(1));
        r.relay.offer(delta(2, 1));
        r.hand(2);
        r.drained();
        r.relay.offer(delta(3, 2));
        r.relay.offer(anchor(4, 3));
        r.relay.flush_for_gate(true, Some(delta(5, 4)));
        assert!(
            r.ids().is_empty() && r.relay.lost_first() == Some(3),
            "as is one queued behind a frame the same flush drops"
        );

        let r = rig(true, true, 100_000, None);
        r.relay.offer(key(1));
        r.relay.offer(delta(2, 1));
        r.hand(2);
        r.relay.flush_for_gate(true, Some(delta(3, 2)));
        let lifted = !r.relay.ungate(false);
        r.relay.hold_sync();
        assert!(
            lifted && !r.relay.has_live_rows() && !r.relay.offer(delta(4, 3)) && r.ids().is_empty(),
            "a re-probed client waits for the lift's key frame"
        );

        let r = rig(true, true, 100_000, None);
        r.relay.offer(key(1));
        r.relay.offer(delta(2, 1));
        r.hand(2);
        r.relay.flush_for_gate(false, Some(delta(3, 2)));
        assert!(
            !r.relay.ungate(true) && !r.relay.has_live_rows(),
            "so does a stalled one"
        );

        let r = rig(true, true, 100_000, None);
        r.relay.offer(key(1));
        r.relay.offer(delta(2, 1));
        r.hand(2);
        r.relay.flush_for_gate(true, Some(delta(3, 2)));
        r.relay.flush_for_gate(true, Some(key(4)));
        assert!(
            !r.relay.ungate(true) && !r.relay.has_live_rows(),
            "and one a key frame was dropped for"
        );
    }

    #[tokio::test]
    async fn an_untracked_stream_skips_ahead_to_the_next_key_frame() {
        reset_clock();
        let r = rig(false, true, 2000, None);
        r.relay.offer(key(1));
        r.hand(1);
        let asks: Vec<bool> = (2..9u16)
            .map(|n| r.relay.offer(chunk(n, n, false, 400, 0, WIRE_H264, false)))
            .collect();
        assert!(
            asks.iter().any(|a| *a),
            "queued to its budget, then asks for a key frame"
        );
        assert!(r.server.reports().is_empty());
        r.relay.offer(chunk(9, 9, false, 400, 0, WIRE_H264, false));
        assert!(r.ids().is_empty(), "its deltas wait for that key frame");
        r.relay
            .flush_for_gate(true, Some(chunk(10, 10, false, 400, 0, WIRE_H264, false)));
        assert!(!r.relay.ungate(true));

        // The budget: a fresh relay waits for a key frame, the frames behind one queue, and a
        // client past its budget skips ahead.
        let r = rig(false, true, 250, None);
        let small = |fid: u16, key: bool| chunk(fid, fid, key, 88, 0, WIRE_H264, false);
        assert!(r.relay.offer(small(0, false)) && r.ids().is_empty());
        assert!(
            !r.relay.offer(small(1, true))
                && !r.relay.offer(small(2, false))
                && r.relay.backlog_len() == 2
        );
        r.relay.offer(small(3, false));
        assert!(
            r.ids().is_empty() && !r.relay.has_live_rows(),
            "past its budget skips ahead"
        );
        assert!(!r.relay.offer(small(4, false)) && r.ids().is_empty());
    }

    #[tokio::test]
    async fn a_lift_asks_for_one_key_frame_and_the_sync_floor_holds() {
        reset_clock();
        let r = rig(true, true, 8 * 1024 * 1024, None);
        r.relay.flush_for_gate(true, None);
        // The lift's own IDR request is the server's; the relay is told one is coming.
        r.relay.hold_sync();
        let asks: Vec<bool> = (10..13u16)
            .map(|n| r.relay.offer(delta(n, n - 1)))
            .collect();
        assert!(
            !asks.iter().any(|a| *a),
            "the deltas ahead of the key frame ask for none"
        );
        r.relay.offer(key(13));
        assert!(!r.relay.offer(delta(14, 13)) && r.relay.backlog_len() == 2);
        r.relay.flush_for_gate(true, None);
        advance(VIDEO_RELAY_SYNC_FLOOR_SECONDS + 0.1);
        assert!(
            r.relay.offer(delta(15, 14)),
            "a relay whose key frame never came asks again after the sync floor"
        );
    }

    /// A frame of `size` bytes as pixelflux frames it.
    fn wire_frame(frame_id: u16, size: usize, is_key: bool) -> RelayItem {
        let mut item = if is_key {
            key(frame_id)
        } else {
            delta(frame_id, frame_id.wrapping_sub(1))
        };
        let mut data = item.data.to_vec();
        data.resize(size, 0);
        item.data = Bytes::from(data);
        item
    }

    /// A 600 KB key frame, then a delta frame a second, into one relay over a socket taking
    /// `rate` bytes a second; whether its client was kept, and how many frames were written.
    async fn relay_run(rate: f64, seconds: f64, liveness: Liveness) -> (bool, usize) {
        let socket = FakeSocket::new(1, rate, 0, liveness);
        let server = Server::new(true);
        let relay = VideoRelay::new(
            server.clone(),
            "primary",
            socket.client.clone(),
            8 * 1024 * 1024,
        );
        relay.start();
        let start = now();
        relay.offer(wire_frame(1, 600 * 1024, true));
        let mut fid = 1;
        while now() - start < seconds {
            tokio::time::sleep(Duration::from_secs(1)).await;
            fid += 1;
            relay.offer(wire_frame(fid, 4 * 1024, false));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let kept = server.died.lock().is_empty() && !socket.client.closed();
        let frames = socket.writes.lock().len();
        relay.stop();
        socket.stop();
        (kept, frames)
    }

    #[tokio::test]
    async fn a_relay_draining_a_key_frame_for_seconds_keeps_its_client() {
        let (kept, frames) = relay_run(150.0 * 1024.0, 6.0, Liveness::default()).await;
        assert!(kept, "600 KB at 150 KB/s drains for about 4 s");
        assert!(
            frames >= 3,
            "and it goes on sending after it: {frames} frames written"
        );
    }

    #[tokio::test]
    async fn a_socket_is_judged_by_progress_not_by_time() {
        let short = Liveness {
            stall_s: 1.5,
            probe_s: 0.1,
        };
        let (kept, _) = relay_run(150.0 * 1024.0, 5.0, short).await;
        assert!(
            kept,
            "a slow socket outlasts a stall bound shorter than its drain"
        );

        let socket = FakeSocket::new(1, 0.0, 0, short);
        let server = Server::new(true);
        let relay = VideoRelay::new(
            server.clone(),
            "primary",
            socket.client.clone(),
            8 * 1024 * 1024,
        );
        relay.start();
        let start = now();
        relay.offer(wire_frame(1, 600 * 1024, true));
        while server.died.lock().is_empty() && now() - start < 8.0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let took = now() - start;
        assert!(
            !server.died.lock().is_empty() && took < 3.0,
            "a socket taking nothing is dropped, after {took:.2} s"
        );
        relay.stop();
        socket.stop();
    }

    #[tokio::test]
    async fn the_relay_judges_the_connection_over_its_windows() {
        reset_clock();
        let r = rig(false, true, 250, None);
        r.relay.set_verdict_window(0.05);
        let frame = |fid: u16, key: bool| chunk(fid, fid, key, 88, 0, WIRE_H264, false);
        let fid = std::cell::Cell::new(0u16);
        let window = |frames: u16, relay: &VideoRelay| {
            for _ in 0..frames {
                fid.set(fid.get() + 1);
                relay.offer(frame(fid.get(), true));
                relay.clear_backlog();
            }
            advance(0.05 + 0.005);
        };
        for _ in 0..3 {
            window(30, &r.relay);
        }
        assert!(
            r.server.verdicts.lock().is_empty(),
            "a client sent every chunk is told nothing"
        );
        for _ in 0..2 {
            r.relay.flush_for_gate(false, None);
            window(30, &r.relay);
        }
        assert!(
            r.server.verdicts.lock().is_empty(),
            "nor one held back as stalled"
        );
        for _ in 0..30 {
            r.relay.flush_for_gate(true, None);
        }
        advance(0.05 + 0.005);
        fid.set(fid.get() + 1);
        r.relay.offer(frame(fid.get(), true));
        assert_eq!(
            r.server.verdicts.lock().clone(),
            vec![(r.page.client.id, true)],
            "chunks the gate holds back make it poor"
        );
        window(30, &r.relay);
        window(30, &r.relay);
        assert_eq!(
            r.server.verdicts.lock().clone(),
            vec![(r.page.client.id, true), (r.page.client.id, false)]
        );
    }
}
