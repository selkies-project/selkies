/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A headless WebRTC client of the server under test, behind a shaped link of its own, that
//! reports what reaches it as it arrives.
//!
//! ```text
//! webrtc-peer WS_URL [UP] [DOWN]
//! ```
//!
//! `WS_URL` is the server's signaling socket (`ws://127.0.0.1:PORT/api/ws`). `UP` and `DOWN` shape
//! the client-to-server and server-to-client directions, each a JSON object (`{}`, the default,
//! passes datagrams through): `rate_bps` (a token bucket of that rate, its queue tail-dropped past
//! 250 kB), `delay_s`, `loss` (a share of datagrams lost, drawn from a stream seeded by `seed`, so a
//! run repeats) and `outage` (`[period_s, length_s]`: nothing gets through for `length_s` of every
//! `period_s`). Every server candidate in the offer, and the client's own in the answer, is
//! replaced by a relay that carries its datagrams through those shapers, and every other candidate
//! is dropped, so all media crosses the shaped link. One shaper serves each direction for every
//! relay, as one access link would.
//!
//! The client announces itself as `client_type` controller in slot -1 (`PEER_CLIENT_TYPE` and
//! `PEER_CLIENT_SLOT` override them), answers with what webrtc-rs supports, and generates the
//! feedback a browser would: NACKs, transport-wide congestion control and receiver reports. It
//! asks for repairs as aiortc's receiver does: on each gap in a video stream, for every packet
//! still missing within the last 128, and never again on a timer. It
//! asks for a picture the way aiortc's receiver does, when its 512-packet video buffer has to
//! drop an incomplete frame to make room or the sequence jumps back a hundred or more, at most
//! once in `PEER_PLI_INTERVAL_S` seconds (none by default); it decodes nothing, so a decoder
//! error never asks.
//!
//! Standard output carries one JSON object per line, `t` in seconds since the program started:
//! - `{"kind":"rtp","t","media":"audio"|"video","ssrc","seq","ts","rtx","osn","head","len","ow"}`
//!   for every RTP packet as it arrives, before any repair or reordering: `rtx` marks a
//!   retransmission, `osn` the sequence number it repairs, `head` the first four payload bytes of
//!   the media it carries (in hex), `len` its payload size, `ow` the one-way delay its
//!   abs-send-time gives in seconds (null without one);
//! - `{"kind":"streams","rtx":{REPAIR:PRIMARY},"fec":[SSRC]}` once the offer names them;
//! - `{"kind":"fb","t","type":"nack","n"}` and `{"kind":"fb","t","type":"pli"}` for every NACK (`n`
//!   the packets it names) and picture request the client sends;
//! - `{"kind":"dc","t","wall","label","text"}` for every text data channel message, `wall` the
//!   system clock in seconds;
//! - `{"kind":"state","t","state"}` as the connection changes;
//! - `{"kind":"shaper","dir","shaped","lost","dropped"}` per direction at the end.
//!
//! Standard input takes one JSON object per line: `{"rate_bps": N}` sets both directions' rate
//! (`{"rate_bps": null}` unshapes them), `{"up": {...}}` or `{"down": {...}}` sets one direction's
//! rate the same way; its end ends the session.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use rtc::interceptor::{Interceptor, Packet, Registry, Slot, StreamInfo, TaggedPacket};
use rtc::peer_connection::RTCPeerConnectionBuilder;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::{
    configure_rtcp_reports, configure_twcc_receiver_only,
};
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::peer_connection::configuration::setting_engine::SettingEngineBuilder;
use rtc::peer_connection::event::{RTCPeerConnectionEvent, RTCTrackEvent};
use rtc::peer_connection::message::{RTCMessage, TaggedRTCMessage};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, RTCDtlsRole, RTCIceCandidate, RTCIceCandidateInit,
};
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::transport_feedbacks::transport_layer_nack::{
    TransportLayerNack, nack_pairs_from_sequence_numbers,
};
use rtc::rtp_transceiver::RTCRtpReceiverId;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpHeaderExtensionCapability, RtpCodecKind, TYPE_RTCP_FB_NACK,
};
use rtc::sansio::Protocol;
use rtc::shared::error::Error;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt as _;
use tokio::net::UdpSocket;
use tokio_websockets::{ClientBuilder, Message};

const ABS_SEND_TIME: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time";
/// A switch's buffer: datagrams past this many queued bytes are dropped, not delayed.
const SHAPER_CAP_BYTES: usize = 250_000;

fn start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn since(at: Instant) -> f64 {
    at.saturating_duration_since(start()).as_secs_f64()
}

fn wall() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// The writer thread's queue and handle: a reader slow to drain the pipe must not hold up the
/// event loop, whose timing is what the lines report.
static OUT: Mutex<Option<(std::sync::mpsc::Sender<String>, std::thread::JoinHandle<()>)>> =
    Mutex::new(None);

/// One line of output.
fn emit(line: &Value) {
    let mut out = OUT.lock().expect("out");
    let (tx, _) = out.get_or_insert_with(|| {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let writer = std::thread::spawn(move || {
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            while let Ok(line) = rx.recv() {
                let _ = writeln!(out, "{line}");
                while let Ok(line) = rx.try_recv() {
                    let _ = writeln!(out, "{line}");
                }
                let _ = out.flush();
            }
        });
        (tx, writer)
    });
    let _ = tx.send(line.to_string());
}

/// Write out everything emitted, before the process ends.
fn drain_output() {
    let taken = OUT.lock().expect("out").take();
    if let Some((tx, writer)) = taken {
        drop(tx);
        let _ = writer.join();
    }
}

/// A 64-bit generator of its own (xorshift*), seeded per direction so a run's losses repeat.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        // 53 random bits, the precision of an f64 in [0, 1).
        #[allow(clippy::cast_precision_loss, reason = "53 bits fit an f64's mantissa")]
        let unit = (x >> 11) as f64 / (1u64 << 53) as f64;
        unit
    }
}

/// One direction of the link: a token bucket's exact departure times on a virtual clock, a delay
/// after them, seeded loss, periodic outages, and a tail-drop cap.
struct Shaper {
    rate_bps: Option<f64>,
    delay: Duration,
    loss: f64,
    outage: Option<(f64, f64)>,
    rng: Rng,
    t0: Instant,
    free_at: Instant,
    queued: usize,
    shaped: u64,
    lost: u64,
    dropped: u64,
}

impl Shaper {
    fn from_json(spec: &Value) -> Self {
        let num = |k: &str| spec.get(k).and_then(Value::as_f64);
        Shaper {
            rate_bps: num("rate_bps").filter(|r| *r > 0.0),
            delay: Duration::from_secs_f64(num("delay_s").unwrap_or(0.0).max(0.0)),
            loss: num("loss").unwrap_or(0.0),
            outage: spec
                .get("outage")
                .and_then(Value::as_array)
                .and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))),
            rng: Rng::new(spec.get("seed").and_then(Value::as_u64).unwrap_or(1)),
            t0: Instant::now(),
            free_at: Instant::now(),
            queued: 0,
            shaped: 0,
            lost: 0,
            dropped: 0,
        }
    }

    /// When a datagram of `len` bytes leaves, and whether it counts against the queue until
    /// `left` is called for it (a datagram of a shaped rate does); `None` when it is lost or
    /// dropped.
    fn push(&mut self, len: usize) -> Option<(Instant, bool)> {
        self.shaped += 1;
        let now = Instant::now();
        if self.loss > 0.0 && self.rng.unit() < self.loss {
            self.lost += 1;
            return None;
        }
        if let Some((period, length)) = self.outage
            && now.saturating_duration_since(self.t0).as_secs_f64() % period < length
        {
            self.lost += 1;
            return None;
        }
        let Some(rate) = self.rate_bps else {
            return Some((now + self.delay, false));
        };
        if self.queued + len > SHAPER_CAP_BYTES {
            self.dropped += 1;
            return None;
        }
        self.queued += len;
        #[allow(clippy::cast_precision_loss, reason = "a datagram's size")]
        let airtime = Duration::from_secs_f64(len as f64 * 8.0 / rate);
        let depart = self.free_at.max(now) + airtime;
        self.free_at = depart;
        Some((depart + self.delay, true))
    }

    fn left(&mut self, len: usize) {
        self.queued = self.queued.saturating_sub(len);
    }
}

type Shared = Arc<Mutex<Shaper>>;

/// Carry a datagram through `shaper` and send it with `send` at its departure time.
fn carry<F>(shaper: &Shared, data: Vec<u8>, send: F)
where
    F: FnOnce(Vec<u8>) + Send + 'static,
{
    let len = data.len();
    let depart = shaper.lock().expect("shaper").push(len);
    let Some((at, counted)) = depart else { return };
    let shaper = Arc::clone(shaper);
    tokio::spawn(async move {
        tokio::time::sleep_until(at.into()).await;
        if counted {
            shaper.lock().expect("shaper").left(len);
        }
        send(data);
    });
}

/// A relay for one candidate: the near side sends to its advertised address, through `up`, to the
/// real `far` address; what comes back passes `down` to whichever near address spoke last.
async fn relay(far: SocketAddr, up: Shared, down: Shared) -> std::io::Result<SocketAddr> {
    let vis = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let out = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    out.connect(far).await?;
    let advertised = vis.local_addr()?;
    let near: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    {
        let (vis, out, near) = (Arc::clone(&vis), Arc::clone(&out), Arc::clone(&near));
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            while let Ok((n, from)) = vis.recv_from(&mut buf).await {
                *near.lock().expect("near") = Some(from);
                let out = Arc::clone(&out);
                carry(&up, buf[..n].to_vec(), move |d| {
                    let _ = out.try_send(&d);
                });
            }
        });
    }
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        while let Ok(n) = out.recv(&mut buf).await {
            let Some(to) = *near.lock().expect("near") else {
                continue;
            };
            let vis = Arc::clone(&vis);
            carry(&down, buf[..n].to_vec(), move |d| {
                let _ = vis.try_send_to(&d, to);
            });
        }
    });
    Ok(advertised)
}

/// `sdp` with every UDP host candidate replaced by a relay toward it and every other candidate
/// dropped; with `own`, also a relay toward that address when the SDP names no candidate.
async fn rewrite_candidates(
    sdp: &str,
    up: &Shared,
    down: &Shared,
    own: Option<SocketAddr>,
) -> Result<String, String> {
    let mut out = Vec::new();
    let mut any = false;
    for line in sdp.split("\r\n") {
        let Some(rest) = line.strip_prefix("a=candidate:") else {
            out.push(line.to_string());
            continue;
        };
        let f: Vec<&str> = rest.split(' ').collect();
        if f.len() < 8 || !f[2].eq_ignore_ascii_case("udp") || f[7] != "host" {
            continue;
        }
        let far: SocketAddr = format!("{}:{}", f[4], f[5])
            .parse()
            .map_err(|e| format!("candidate address {}:{}: {e}", f[4], f[5]))?;
        let at = relay(far, Arc::clone(up), Arc::clone(down))
            .await
            .map_err(|e| e.to_string())?;
        let tail = f[8..].join(" ");
        out.push(format!(
            "a=candidate:{} {} UDP {} {} {} typ host{}{}",
            f[0],
            f[1],
            f[3],
            at.ip(),
            at.port(),
            if tail.is_empty() { "" } else { " " },
            tail
        ));
        any = true;
    }
    if !any && let Some(own) = own {
        let at = relay(own, Arc::clone(up), Arc::clone(down))
            .await
            .map_err(|e| e.to_string())?;
        let line = format!(
            "a=candidate:1 1 UDP 2130706431 {} {} typ host",
            at.ip(),
            at.port()
        );
        // Every media section takes the candidate (they are bundled), after its connection line.
        let mut with = Vec::new();
        for l in out {
            let media = l.starts_with("m=");
            with.push(l);
            if media {
                with.push(line.clone());
            }
        }
        return Ok(with.join("\r\n"));
    }
    Ok(out.join("\r\n"))
}

/// The offer with a placeholder SSRC on each audio section that names none. The server leaves
/// audio's SSRC unsignaled for the page to route by MID, and webrtc-rs refuses a section with
/// neither an SSRC nor RIDs beside others; the peer reads audio by its payload types
/// (`Streams::payload`), so the placeholder only gets the description accepted.
fn declare_unsignaled(sdp: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut section: Vec<&str> = Vec::new();
    let mut placeholder = 0u32;
    let mut flush = |section: &mut Vec<&str>, out: &mut Vec<String>| {
        let audio = section.first().is_some_and(|l| l.starts_with("m=audio"));
        out.extend(section.iter().map(|l| (*l).to_owned()));
        if audio && !section.iter().any(|l| l.starts_with("a=ssrc:")) {
            placeholder += 1;
            out.push(format!("a=ssrc:{placeholder} cname:unsignaled"));
        }
        section.clear();
    };
    for line in sdp.split("\r\n").filter(|l| !l.is_empty()) {
        if line.starts_with("m=") {
            flush(&mut section, &mut out);
        }
        section.push(line);
    }
    flush(&mut section, &mut out);
    out.push(String::new());
    out.join("\r\n")
}

/// What the offer says each SSRC carries: its media and, for a retransmission or FEC stream of a
/// section the server sends in, the SSRC it repairs; and the abs-send-time extension's id per
/// media.
#[derive(Default)]
struct Streams {
    kind: HashMap<u32, &'static str>,
    /// The media of each payload type, for a stream the offer leaves unsignaled (audio's).
    payload: HashMap<u8, &'static str>,
    rtx: HashMap<u32, u32>,
    fec: Vec<u32>,
    abs_send_time: HashMap<&'static str, u8>,
}

impl Streams {
    fn from_offer(sdp: &str) -> Self {
        let mut s = Streams::default();
        let mut media = "";
        // A section's repair groups count once its direction is known: the offer's receive-only
        // sections (the microphone's and the webcam's) name streams the server never sends.
        let mut sends = true;
        let mut rtx: Vec<(u32, u32)> = Vec::new();
        let mut fec: Vec<u32> = Vec::new();
        let flush =
            |s: &mut Streams, sends: bool, rtx: &mut Vec<(u32, u32)>, fec: &mut Vec<u32>| {
                if sends {
                    s.rtx.extend(rtx.iter().copied());
                    s.fec.append(fec);
                }
                rtx.clear();
                fec.clear();
            };
        for line in sdp.lines().map(str::trim_end) {
            if let Some(m) = line.strip_prefix("m=") {
                flush(&mut s, sends, &mut rtx, &mut fec);
                sends = true;
                media = if m.starts_with("audio") {
                    "audio"
                } else if m.starts_with("video") {
                    "video"
                } else {
                    ""
                };
                if !media.is_empty() {
                    for pt in m.split(' ').skip(3).filter_map(|x| x.parse().ok()) {
                        s.payload.entry(pt).or_insert(media);
                    }
                }
            } else if line == "a=recvonly" || line == "a=inactive" {
                sends = false;
            } else if let Some(rest) = line.strip_prefix("a=ssrc:") {
                if let Some(ssrc) = rest.split(' ').next().and_then(|x| x.parse().ok())
                    && !media.is_empty()
                {
                    s.kind.insert(ssrc, media);
                }
            } else if let Some(rest) = line.strip_prefix("a=ssrc-group:FID ") {
                let ids: Vec<u32> = rest.split(' ').filter_map(|x| x.parse().ok()).collect();
                if let [primary, repair] = ids[..] {
                    rtx.push((repair, primary));
                }
            } else if let Some(rest) = line.strip_prefix("a=ssrc-group:FEC-FR ") {
                let ids: Vec<u32> = rest.split(' ').filter_map(|x| x.parse().ok()).collect();
                if let [_, repair] = ids[..] {
                    fec.push(repair);
                }
            } else if let Some(rest) = line.strip_prefix("a=extmap:")
                && rest.contains(ABS_SEND_TIME)
                && let Some(id) = rest.split([' ', '/']).next().and_then(|x| x.parse().ok())
                && !media.is_empty()
            {
                s.abs_send_time.insert(media, id);
            }
        }
        flush(&mut s, sends, &mut rtx, &mut fec);
        s
    }
}

/// The current NTP time's 24 abs-send-time bits: 6.18 fixed-point seconds.
fn ntp_24() -> u32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() + 2_208_988_800;
    let frac = (u64::from(now.subsec_nanos()) << 32) / 1_000_000_000;
    let ntp = (secs << 32) | frac;
    u32::try_from((ntp >> 14) & 0xFF_FFFF).unwrap_or(0)
}

/// aiortc's video jitter buffer, kept only for when it would ask for a picture: a packet at or
/// past the buffer's end drops whole frames from its front to make room, and one a hundred or
/// more behind its front empties it.
struct Jitter {
    origin: Option<u16>,
    slots: Vec<Option<u32>>,
}

impl Jitter {
    const CAPACITY: u16 = 512;
    const MAX_MISORDER: u16 = 100;

    fn new() -> Self {
        Jitter {
            origin: None,
            slots: vec![None; usize::from(Self::CAPACITY)],
        }
    }

    fn pos(seq: u16) -> usize {
        usize::from(seq % Self::CAPACITY)
    }

    /// Take a media packet; whether the receiver would ask for a picture.
    fn add(&mut self, seq: u16, ts: u32) -> bool {
        let mut pli = false;
        let (mut delta, misorder) = match self.origin {
            None => {
                self.origin = Some(seq);
                (0, 0)
            }
            Some(origin) => (seq.wrapping_sub(origin), origin.wrapping_sub(seq)),
        };
        if misorder < delta {
            if misorder < Self::MAX_MISORDER {
                return false;
            }
            self.remove(Self::CAPACITY);
            self.origin = Some(seq);
            delta = 0;
            pli = true;
        }
        if delta >= Self::CAPACITY {
            if self.smart_remove(delta - Self::CAPACITY + 1) {
                self.origin = Some(seq);
            }
            pli = true;
        }
        self.slots[Self::pos(seq)] = Some(ts);
        self.remove_frame();
        pli
    }

    /// Hand the first complete frame on: everything before the first change of timestamp.
    fn remove_frame(&mut self) {
        let Some(origin) = self.origin else { return };
        let mut first: Option<u32> = None;
        for count in 0..Self::CAPACITY {
            let Some(ts) = self.slots[Self::pos(origin.wrapping_add(count))] else {
                return;
            };
            match first {
                None => first = Some(ts),
                Some(f) if f != ts => {
                    self.remove(count);
                    return;
                }
                Some(_) => {}
            }
        }
    }

    fn remove(&mut self, count: u16) {
        let Some(mut origin) = self.origin else {
            return;
        };
        for _ in 0..count {
            self.slots[Self::pos(origin)] = None;
            origin = origin.wrapping_add(1);
        }
        self.origin = Some(origin);
    }

    /// Drop at least `count` packets from the front, and the rest of the frame the last of them
    /// belongs to; whether that emptied the whole buffer.
    fn smart_remove(&mut self, count: u16) -> bool {
        let Some(mut origin) = self.origin else {
            return false;
        };
        let mut first: Option<u32> = None;
        let mut emptied = false;
        for i in 0..Self::CAPACITY {
            let pos = Self::pos(origin);
            if let Some(ts) = self.slots[pos] {
                if i >= count && first != Some(ts) {
                    break;
                }
                first = Some(ts);
            }
            self.slots[pos] = None;
            origin = origin.wrapping_add(1);
            if i == Self::CAPACITY - 1 {
                emptied = true;
            }
        }
        self.origin = Some(origin);
        emptied
    }
}

/// Whether RTP sequence number `a` is after `b`, across the 16-bit wrap.
fn seq_after(a: u16, b: u16) -> bool {
    (a < b && b - a > 0x8000) || (a > b && a - b < 0x8000)
}

/// aiortc's NACK generator for one video stream: a packet past the highest one seen marks every
/// number between as missing, and each such gap asks for all that are still missing; a packet
/// that arrives, or one that falls more than 128 behind the highest, is asked for no more.
#[derive(Default)]
struct Nacks {
    max_seq: Option<u16>,
    missing: std::collections::BTreeSet<u16>,
}

impl Nacks {
    const HISTORY: u16 = 128;

    /// Take a media packet's sequence number; whether it opened a gap.
    fn add(&mut self, seq: u16) -> bool {
        let Some(max) = self.max_seq else {
            self.max_seq = Some(seq);
            return false;
        };
        let mut missed = false;
        if seq_after(seq, max) {
            let mut at = max.wrapping_add(1);
            while seq_after(seq, at) {
                self.missing.insert(at);
                missed = true;
                at = at.wrapping_add(1);
            }
            self.max_seq = Some(seq);
        } else {
            self.missing.remove(&seq);
        }
        let floor = self.max_seq.unwrap_or(seq).wrapping_sub(Self::HISTORY);
        self.missing.retain(|m| !seq_after(floor, *m));
        missed
    }
}

/// What the receiver asks the sender for, handed from the tap to the event loop: a picture
/// (`interval` spaces those, `PEER_PLI_INTERVAL_S`, none by default as aiortc's receiver) and
/// the packets a gap left missing.
#[derive(Default)]
struct Pli {
    wanted: Option<u32>,
    last: Option<Instant>,
    interval: Duration,
    nack: Option<(u32, Vec<u16>)>,
}

/// The tap in the wire-most slot: every inbound RTP packet as it arrived, retransmissions under
/// their own SSRC with the sequence number they repair still in front of their payload.
struct Tap {
    streams: Arc<Mutex<Streams>>,
    jitter: Jitter,
    nacks: Nacks,
    pli: Arc<Mutex<Pli>>,
    read: VecDeque<TaggedPacket>,
    write: VecDeque<TaggedPacket>,
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for Tap {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtp(p) = &msg.message.packet {
            let streams = self.streams.lock().expect("streams");
            let ssrc = p.header.ssrc;
            let repaired = streams.rtx.get(&ssrc).copied();
            let media = streams
                .kind
                .get(&repaired.unwrap_or(ssrc))
                .or_else(|| streams.payload.get(&p.header.payload_type))
                .copied()
                .unwrap_or("unknown");
            let payload = &p.payload[..];
            let (osn, head) = if repaired.is_some() && payload.len() >= 2 {
                (
                    Some(u16::from_be_bytes([payload[0], payload[1]])),
                    &payload[2..payload.len().min(6)],
                )
            } else {
                (None, &payload[..payload.len().min(4)])
            };
            let ow = streams
                .abs_send_time
                .get(media)
                .and_then(|id| p.header.get_extension(*id))
                .filter(|v| v.len() >= 3)
                .and_then(|v| {
                    let ast = (u32::from(v[0]) << 16) | (u32::from(v[1]) << 8) | u32::from(v[2]);
                    let diff = ntp_24().wrapping_sub(ast) & 0xFF_FFFF;
                    // Past half the 64 s wrap is a wrap artifact, not a delay.
                    (diff < 0x80_0000).then(|| f64::from(diff) / 262_144.0)
                });
            let primary = repaired.unwrap_or(ssrc);
            // aiortc drops a retransmission too short to name the packet it repairs.
            let unwrappable = repaired.is_none() || payload.len() >= 2;
            if media == "video" && unwrappable && !streams.fec.contains(&primary) {
                let seq = osn.unwrap_or(p.header.sequence_number);
                if self.nacks.add(seq) {
                    // Numbers in order, as aiortc sorts them, the wrap aside.
                    let lost: Vec<u16> = self.nacks.missing.iter().copied().collect();
                    self.pli.lock().expect("pli").nack = Some((primary, lost));
                }
                if self.jitter.add(seq, p.header.timestamp) {
                    let mut pli = self.pli.lock().expect("pli");
                    if pli
                        .last
                        .is_none_or(|at| msg.now.saturating_duration_since(at) >= pli.interval)
                    {
                        pli.last = Some(msg.now);
                        pli.wanted = Some(primary);
                    }
                }
            }
            let hex: String = head.iter().map(|b| format!("{b:02x}")).collect();
            emit(&json!({
                "kind": "rtp", "t": since(msg.now), "media": media, "ssrc": ssrc,
                "seq": p.header.sequence_number, "ts": p.header.timestamp,
                "rtx": repaired.is_some(), "osn": osn, "head": hex, "len": payload.len(), "ow": ow,
            }));
        }
        self.read.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            for p in packets {
                if let Some(nack) = p.as_any().downcast_ref::<TransportLayerNack>() {
                    let n: usize = nack.nacks.iter().map(|pair| pair.packet_list().len()).sum();
                    emit(&json!({"kind": "fb", "t": since(msg.now), "type": "nack", "n": n}));
                } else if p.as_any().is::<PictureLossIndication>() {
                    emit(&json!({"kind": "fb", "t": since(msg.now), "type": "pli"}));
                }
            }
        }
        self.write.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write.pop_front()
    }
}

impl Interceptor for Tap {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}

/// Set the rate of `shaper` from a control line's value.
fn set_rate(shaper: &Shared, value: &Value) {
    let rate = value
        .get("rate_bps")
        .map_or(value.as_f64(), Value::as_f64)
        .filter(|r| *r > 0.0);
    shaper.lock().expect("shaper").rate_bps = rate;
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    let ended = run().await;
    drain_output();
    ended
}

#[allow(
    clippy::too_many_lines,
    reason = "one event loop over the socket, signaling and timers"
)]
async fn run() -> Result<(), String> {
    selkies_suites::die_with_parent();
    let _ = start();
    let args: Vec<String> = std::env::args().collect();
    let url = args
        .get(1)
        .ok_or("usage: webrtc-peer WS_URL [UP] [DOWN]")?
        .clone();
    let spec = |i: usize| -> Result<Value, String> {
        args.get(i).map_or(Ok(json!({})), |s| {
            serde_json::from_str(s).map_err(|e| format!("shaper {s}: {e}"))
        })
    };
    let up: Shared = Arc::new(Mutex::new(Shaper::from_json(&spec(2)?)));
    let down: Shared = Arc::new(Mutex::new(Shaper::from_json(&spec(3)?)));
    let client_type = std::env::var("PEER_CLIENT_TYPE").unwrap_or_else(|_| "controller".into());
    let client_slot: i64 = std::env::var("PEER_CLIENT_SLOT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1);

    let socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let local_addr = socket.local_addr().map_err(|e| e.to_string())?;
    let streams = Arc::new(Mutex::new(Streams::default()));
    let pli = Arc::new(Mutex::new(Pli {
        interval: Duration::from_secs_f64(
            std::env::var("PEER_PLI_INTERVAL_S")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0)
                .max(0.0),
        ),
        ..Pli::default()
    }));
    let mut video_receiver: Option<RTCRtpReceiverId> = None;

    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .map_err(|e| e.to_string())?;
    for kind in [RtpCodecKind::Audio, RtpCodecKind::Video] {
        media_engine
            .register_header_extension(
                RTCRtpHeaderExtensionCapability {
                    uri: ABS_SEND_TIME.to_string(),
                },
                kind,
                None,
            )
            .map_err(|e| e.to_string())?;
    }
    // The feedback a browser negotiates, NACK and PLI among it, but no NACK generator of rtc's:
    // the tap asks for repairs as aiortc's receiver does, once per gap, never on a timer.
    for parameter in ["", "pli"] {
        media_engine.register_feedback(
            RTCPFeedback {
                typ: TYPE_RTCP_FB_NACK.to_owned(),
                parameter: parameter.to_owned(),
            },
            RtpCodecKind::Video,
        );
    }
    let registry = configure_twcc_receiver_only(Registry::new(), &mut media_engine)
        .map_err(|e| e.to_string())?;
    let registry = configure_rtcp_reports(registry).with(
        Slot::Custom(0),
        Tap {
            streams: Arc::clone(&streams),
            jitter: Jitter::new(),
            nacks: Nacks::default(),
            pli: Arc::clone(&pli),
            read: VecDeque::new(),
            write: VecDeque::new(),
        },
    );
    let mut pc = RTCPeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        // The server opens the channels as the SCTP client; rtc accepts an INIT only as the DTLS
        // server, so the answer takes that role.
        .with_setting_engine(
            SettingEngineBuilder::new()
                .with_answering_dtls_role(RTCDtlsRole::Server)
                .build(),
        )
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build(Instant::now())
        .map_err(|e| e.to_string())?;

    let uri = url.parse().map_err(|e| format!("{url}: {e}"))?;
    let (mut ws, _) = ClientBuilder::from_uri(uri)
        .connect()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    ws.send(Message::text(format!(
        "HELLO client {}",
        json!({"client_type": client_type, "client_slot": client_slot})
    )))
    .await
    .map_err(|e| e.to_string())?;

    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut retry_session: Option<Instant> = None;
    let mut buf = vec![0u8; 2048];
    loop {
        while let Some(out) = pc.poll_write() {
            let _ = socket.send_to(&out.message, out.transport.peer_addr).await;
        }
        while let Some(event) = pc.poll_event() {
            if std::env::var_os("PEER_DEBUG").is_some() {
                eprintln!("event {event:?}");
            }
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                    emit(
                        &json!({"kind": "state", "t": since(Instant::now()), "state": state.to_string()}),
                    );
                }
                RTCPeerConnectionEvent::OnTrack(RTCTrackEvent::OnOpen(init))
                    if streams.lock().expect("streams").kind.get(&init.ssrc) == Some(&"video") =>
                {
                    video_receiver = Some(init.receiver_id);
                }
                _ => {}
            }
        }
        while let Some(TaggedRTCMessage { message, .. }) = pc.poll_read() {
            if let RTCMessage::DataChannelMessage(id, msg) = message
                && msg.is_string
            {
                let label = pc
                    .data_channel(id)
                    .map(|c| c.label().to_string())
                    .unwrap_or_default();
                emit(&json!({
                    "kind": "dc", "t": since(Instant::now()), "wall": wall(), "label": label,
                    "text": String::from_utf8_lossy(&msg.data),
                }));
            }
        }
        let nack = pli.lock().expect("pli").nack.take();
        if let (Some((media_ssrc, lost)), Some(id)) = (nack, video_receiver)
            && let Some(mut receiver) = pc.rtp_receiver(id)
        {
            let request = TransportLayerNack {
                sender_ssrc: 0,
                media_ssrc,
                nacks: nack_pairs_from_sequence_numbers(&lost),
            };
            let _ = receiver.write_rtcp(Instant::now(), vec![Box::new(request)]);
        }
        let wanted = pli.lock().expect("pli").wanted.take();
        if let (Some(media_ssrc), Some(id)) = (wanted, video_receiver)
            && let Some(mut receiver) = pc.rtp_receiver(id)
        {
            let request = PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc,
            };
            let _ = receiver.write_rtcp(Instant::now(), vec![Box::new(request)]);
            continue;
        }
        let deadline = pc
            .poll_timeout()
            .unwrap_or_else(|| Instant::now() + Duration::from_millis(100))
            .min(retry_session.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)));
        tokio::select! {
            biased;
            line = stdin.next_line() => match line {
                Ok(Some(line)) => {
                    let Ok(cmd) = serde_json::from_str::<Value>(&line) else { continue };
                    if cmd.get("rate_bps").is_some() {
                        set_rate(&up, &cmd);
                        set_rate(&down, &cmd);
                    }
                    if let Some(v) = cmd.get("up") { set_rate(&up, v); }
                    if let Some(v) = cmd.get("down") { set_rate(&down, v); }
                }
                _ => break,
            },
            msg = ws.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Some(text) = msg.as_text() else { continue };
                if text == "HELLO" {
                    let _ = ws.send(Message::text("SESSION server")).await;
                } else if text == "ERROR peer server not found" {
                    // The server's own peer can register after the client's HELLO.
                    retry_session = Some(Instant::now() + Duration::from_secs(1));
                } else if text.starts_with("SESSION") || text.starts_with("ERROR") {
                } else if let Some((peer, payload)) = text.split_once(' ') {
                    let Ok(obj) = serde_json::from_str::<Value>(payload) else { continue };
                    if let Some(sdp) = obj.get("sdp") {
                        if sdp.get("type").and_then(Value::as_str) != Some("offer") { continue }
                        let offer = sdp.get("sdp").and_then(Value::as_str).unwrap_or_default();
                        let parsed = Streams::from_offer(offer);
                        let rtx: serde_json::Map<String, Value> = parsed
                            .rtx
                            .iter()
                            .map(|(repair, primary)| (repair.to_string(), json!(primary)))
                            .collect();
                        emit(&json!({"kind": "streams", "rtx": rtx, "fec": parsed.fec}));
                        *streams.lock().expect("streams") = parsed;
                        let offer = declare_unsignaled(offer);
                        let offer = rewrite_candidates(&offer, &up, &down, None).await?;
                        pc.set_remote_description(Instant::now(),
                            RTCSessionDescription::offer(offer.clone()).map_err(|e| e.to_string())?)
                            .map_err(|e| e.to_string())?;
                        let candidate = CandidateHostConfig {
                            base_config: CandidateConfig {
                                network: "udp".to_owned(),
                                address: local_addr.ip().to_string(),
                                port: local_addr.port(),
                                component: 1,
                                ..Default::default()
                            },
                            ..Default::default()
                        }
                        .new_candidate_host()
                        .map_err(|e| e.to_string())?;
                        pc.add_local_candidate(RTCIceCandidate::from(&candidate).to_json().map_err(|e| e.to_string())?)
                            .map_err(|e| e.to_string())?;
                        let answer = pc.create_answer(None).map_err(|e| e.to_string())?;
                        pc.set_local_description(Instant::now(), answer).map_err(|e| e.to_string())?;
                        let local = pc.local_description().ok_or("no local description")?.sdp.clone();
                        // The server sends to these relays, so what enters them is its media:
                        // `down` shapes it on the way in.
                        let answer = rewrite_candidates(&local, &down, &up, Some(local_addr)).await?;
                        if std::env::var_os("PEER_DEBUG").is_some() {
                            eprintln!("OFFER\n{offer}\nANSWER\n{answer}");
                        }
                        let reply = json!({"sdp": {"type": "answer", "sdp": answer}});
                        let _ = ws.send(Message::text(format!("{peer} {reply}"))).await;
                    } else if let Some(ice) = obj.get("ice") {
                        let candidate = ice.get("candidate").and_then(Value::as_str).unwrap_or_default();
                        if !candidate.is_empty() {
                            let init = RTCIceCandidateInit {
                                candidate: candidate.to_string(),
                                sdp_mline_index: Some(
                                    u16::try_from(ice.get("sdpMLineIndex").and_then(Value::as_u64).unwrap_or(0)).unwrap_or(0),
                                ),
                                ..Default::default()
                            };
                            let _ = pc.add_remote_candidate(init);
                        }
                    }
                }
            },
            res = socket.recv_from(&mut buf) => {
                let Ok((n, peer_addr)) = res else { break };
                let _ = pc.handle_read(TaggedBytesMut {
                    now: Instant::now(),
                    transport: TransportContext {
                        local_addr,
                        peer_addr,
                        ecn: None,
                        transport_protocol: TransportProtocol::UDP,
                    },
                    message: BytesMut::from(&buf[..n]),
                });
            },
            () = tokio::time::sleep_until(deadline.into()) => {
                let now = Instant::now();
                if retry_session.is_some_and(|at| at <= now) {
                    retry_session = None;
                    let _ = ws.send(Message::text("SESSION server")).await;
                }
                let _ = pc.handle_timeout(now);
            },
        }
    }
    for (dir, shaper) in [("up", &up), ("down", &down)] {
        let s = shaper.lock().expect("shaper");
        emit(
            &json!({"kind": "shaper", "dir": dir, "shaped": s.shaped, "lost": s.lost, "dropped": s.dropped}),
        );
    }
    let _ = pc.close();
    let _ = ws.close().await;
    Ok(())
}
