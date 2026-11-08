/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * This file incorporates work covered by the following copyright and
 * permission notice:
 *
 *   Copyright (c) Jeremy Lainé.
 *   All rights reserved.
 *
 *   Redistribution and use in source and binary forms, with or without
 *   modification, are permitted provided that the following conditions are met:
 *
 *       * Redistributions of source code must retain the above copyright notice,
 *       this list of conditions and the following disclaimer.
 *       * Redistributions in binary form must reproduce the above copyright notice,
 *       this list of conditions and the following disclaimer in the documentation
 *       and/or other materials provided with the distribution.
 *       * Neither the name of aiortc nor the names of its contributors may
 *       be used to endorse or promote products derived from this software without
 *       specific prior written permission.
 *
 *   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
 *   ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
 *   WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 *   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
 *   FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 *   DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 *   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 *   CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 *   OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
 *   OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */

//! The RTP sender of one track: frames pixelflux or pcmflux encoded go in, paced RTP goes
//! out, and the peer's RTCP comes back to it.
//!
//! Each frame (`push_frame`) is packetized by the negotiated codec and every datagram of
//! it is built before the first is sent, since the frame is decodable only once its last
//! packet is in. Each packet carries the header extensions the peer negotiated:
//! abs-send-time, mid, a transport-wide sequence number of its own (retransmissions take
//! fresh ones), playout-delay (0, 0), the color space on every packet of a key frame for
//! the codecs whose bitstream cannot state it (`rtp_color_space`), video-timing on the last
//! packet of a frame about five times a second (`video_timing_legs`), abs-capture-time on
//! the first packet of a key frame and once a second, and the dependency descriptor on
//! every packet where the encoder tracks references (`describe`): the frame's number on the
//! wire and how far back it predicts.
//!
//! FlexFEC repairs are built beside the frame's media packets and sent in the same burst: a
//! group closes at every frame end or at `FEC_GROUP_PACKETS`, takes the repairs `fec_repairs`
//! grants it at the density `steer_fec` sets from the smoothed loss, and the repairs are
//! interleaved so `n` repairs recover `n` losses one XOR at a time. The history
//! (`RtpHistory`) is told the instant a repair reaches the wire, the pacer's queue included.
//!
//! A NACK (`handle_rtcp`) is answered from the history: a first NACK retransmits the packet
//! twice back to back, since lost with the original it would cost the frame a second NACK a
//! round trip later; a second NACK for a packet still held, arriving a round trip after the
//! last repair went out, means the frame is lost to the peer, and `SenderHooks::lost_frame`
//! names it once so the encoder takes it out of its references. A NACK for a packet no
//! longer held, or past repair, is answered with a key frame only where no resync is
//! possible (`resync_held`, `past_repair`). PLI and FIR are gated by `still_request` (no key
//! frame for a receiver's wait on a still screen) and `resync_held`.
//!
//! Where the encoder names each frame's reference and the peer reads the dependency
//! descriptor, a peer that does not own its display is served as a selective forwarding
//! unit serves a receiver on a narrow link (`forward`): a delta frame finding
//! `RESYNC_ROOM_FRAMES` of the peer's frames in its pacer, or `RESYNC_LAG_S` of wait there,
//! is left out with the frames predicting from it, before any is numbered. The encoder is
//! told the run's first frame is lost once the peer has room, or sooner where waiting would
//! cost a key frame (`repair`), and the peer resumes on the first frame predicting past the
//! run. A pacer GOP reset is handed to the sender the same way (`pacer_resync`), a loss past
//! repair is resynced from the newest frame the peer holds (`resync_held`), and which frames
//! a peer holds is read from transport-cc (`confirm`). An H.264 peer without the descriptor
//! is sent a skipped picture in place of each frame left out (`stand_in`).
//!
//! A still screen's path probe pads the video stream (`send_padding`) by resending its last
//! packets on the RTX stream, as libwebrtc's payload padding does: of those carrying
//! `PADDING_SOURCE_BYTES` or more, the newest `PADDING_SOURCES`, kept past the retransmission
//! horizon since a still screen sends none for long. Each is numbered for transport-cc only
//! once the pacer admits it from its credit with nothing queued, so none is read as lost and
//! none waits ahead of a frame; the peer discards them as duplicates.
//!
//! The sender does no I/O and keeps no clock: every call takes `now`, packets leave through
//! `SenderTransport`, and what the engine must learn arrives through `SenderHooks`.

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use rand::RngExt;

use super::clock::ntp_from_monotonic_ns;
use super::codecs::{Codec, Packetizer, h264, h264_skip};
use super::fec::{RepairHeader, build_flexfec_03, interleaved};
use super::history::RtpHistory;
use super::pacer::{Class, FRAME_ENDS_MAX, Pacer, PacerEvent, Verdict};
use super::packet::{
    HeaderExtensionsMap, Payload, RtpPacket, dependency_descriptor, random_sequence_number,
    random32, wrap_rtx,
};
use super::params::{CodecParameters, SendParameters};
use super::rtcp::{
    RTCP_PSFB_APP, RTCP_PSFB_FIR, RTCP_PSFB_PLI, RTCP_RTPFB_NACK, RTCP_RTPFB_TWCC, RtcpPacket,
    SenderInfo, SourceInfo, XrPacket, unpack_remb_fci,
};
use super::twcc::TwccSender;

pub const RTT_ALPHA: f64 = 0.85;
/// Receive-only reporters answered per sender report (libwebrtc answers at most 50 per XR).
pub const RRTR_REPORTERS_MAX: usize = 50;
/// How long a peer may send nothing at all before this sender stops: a browser sends
/// feedback for whatever it receives and checks consent on its path every few seconds even
/// while a still screen sends it nothing, so a peer silent this long is gone.
pub const PEER_SILENCE_S: f64 = 10.0;
/// A libwebrtc receiver with no frame to decode for three times the offer's rtx-time
/// (375 ms), and a packet in the last five seconds, asks for a key frame, and again each
/// 375 ms: a still screen sends nothing, so each still was answered with a key frame and the
/// cleanup after it. A request this long past the round trip after an acknowledged frame,
/// with nothing lost since, is that wait; one this soon after the last is a failed decode.
pub const STILL_REQUEST_S: f64 = 0.25;
pub const RESYNC_ROOM_FRAMES: usize = 3;
pub const RESYNC_LAG_S: f64 = 0.1;
pub const RESYNC_REACH_FRAMES: usize = 5;
pub const RESYNC_WRAP_LEAD: usize = 2;
pub const RESYNC_FAR_FRAMES: usize = 12;
pub const RESYNC_FAR_S: f64 = 1.0;
pub const RESYNC_LOSS_S: f64 = 0.5;
pub const RESYNC_S: f64 = 1.0;
/// Where the encoder pins an anchor every peer holds, it predicts past a run of any depth on
/// the report, so a run waits this long for room.
pub const RESYNC_ANCHORED_S: f64 = 4.0;
pub const RESYNC_DARK_S: f64 = 0.25;
/// An anchor goes to a peer without room only while the queue standing in front of it is
/// under this: on a link too narrow for the anchors alone they would never let it drain.
pub const RESYNC_ANCHOR_S: f64 = 0.25;
pub const HELD_S: f64 = 8.0;
pub const FRAME_NUMBERS_MEMORY: usize = 4096;
/// The shortest H.264 frame_num range, which every longer one is a multiple of.
pub const FRAME_NUM_WRAP: usize = 16;
/// Media packets per FlexFEC group at most; a group also closes with its frame.
pub const FEC_GROUP_PACKETS: usize = 10;
/// Smoothed loss past which every FlexFEC group takes its repairs, read on windows no queue
/// stood in for `FEC_CLEAR_WINDOWS` before them, so a queue's own overflow never counts.
pub const FEC_FULL_LOSS: f64 = 0.002;
pub const FEC_CLEAR_WINDOWS: u32 = 3;
/// Where congestion control bounds them (a target a queue cut), repairs (retransmissions and
/// FlexFEC) take at most `REPAIR_SHARE` of the rate it steered the display to, over
/// `REPAIR_WINDOW_S` (libwebrtc's retransmission rate limiter): a path that loses packets because
/// it is full is sent no more than it was asked to carry (`set_repair_budget`).
pub const REPAIR_SHARE: f64 = 1.0;
pub const REPAIR_WINDOW_S: f64 = 0.5;
/// The share of the repair budget FlexFEC leaves to retransmissions (`repair_room`).
pub const FEC_RESERVE: f64 = 0.5;
/// Where congestion control measures what the path delivers, a key frame asked of the encoder
/// answers the requests after it for `KEY_ASK_S`, and one on the wire until its last packet is
/// acknowledged or has had the round trip, `KEY_MARGIN_S` and the time its bytes take at that (at
/// least `KEY_FLOOR_BPS`) (`key_on_its_way`).
pub const KEY_ASK_S: f64 = 0.3;
pub const KEY_FLOOR_BPS: f64 = 100_000.0;
pub const KEY_MARGIN_S: f64 = 0.1;
const UNHELD_MAX: usize = 1024;
const IN_FLIGHT_MAX: usize = 64;
/// Padding resends the newest of the stream's packets carrying this much or more.
pub const PADDING_SOURCES: usize = 4;
pub const PADDING_SOURCE_BYTES: usize = 900;
/// Room left for a padding packet's RTP header and extensions over its payload.
pub const PADDING_HEADER_BYTES: usize = 64;

/// The color signal a stream was converted with, as the ITU-T H.273 codes the RTP
/// color-space header extension carries, for the two codecs whose bitstream cannot state
/// it: BT.709 primaries, transfer, and matrix at limited range, with the BT.601 matrix for
/// VP8, which is held to the only one a keyframe header's single color-space bit can name.
/// H.264, H.265, and AV1 are left to their own headers, which carry the range as well.
pub fn rtp_color_space(codec: Codec) -> Option<(u8, u8, u8, u8)> {
    match codec {
        Codec::Vp8 => Some((1, 1, 6, 1)),
        Codec::Vp9 => Some((1, 1, 1, 1)),
        _ => None,
    }
}

/// The video-timing extension of a frame packetized at `now_ns` (`CLOCK_MONOTONIC`): the
/// flags byte, timer-triggered, then six millisecond legs from the capture. With `timing`
/// as the capture library stamped it the encode legs are real and the packetization leg is
/// the frame's whole age; without it, the encode legs are unknown (0) and the packetization
/// leg is the frame's time in the sender, `arrival_delta_ms`. Pacer exit repeats
/// packetization, since the stamp is taken ahead of the pacer, and the two network legs
/// stay 0, as a sender that is not a middlebox leaves them.
pub fn video_timing_legs(
    timing: Option<FrameTiming>,
    now_ns: i64,
    arrival_delta_ms: u16,
) -> (u8, u16, u16, u16, u16, u16, u16) {
    match timing {
        Some(t) if t.capture_ns > 0 => {
            let leg =
                |instant_ns: i64| ((instant_ns - t.capture_ns) / 1_000_000).clamp(0, 0xFFFF) as u16;
            let packetized = leg(now_ns);
            (
                0x01,
                leg(t.encode_start_ns),
                leg(t.encode_end_ns),
                packetized,
                packetized,
                0,
                0,
            )
        }
        _ => (0x01, 0, 0, arrival_delta_ms, arrival_delta_ms, 0, 0),
    }
}

/// A video frame's capture, encode-start, and encode-end instants in `CLOCK_MONOTONIC`
/// nanoseconds as the capture library stamped them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameTiming {
    pub capture_ns: i64,
    pub encode_start_ns: i64,
    pub encode_end_ns: i64,
}

/// One pre-encoded media sample on the send path: the encoder's buffer, referenced and
/// never copied, with its RTP timestamp in the codec's clock. `keyframe` says whether the
/// sample decodes on its own, as the encoder reported it. `dependency` is a video frame's
/// own id and the id of the frame it predicts from (`None` for one predicting from nothing),
/// where the encoder tracks its references; `None` where it cannot. `codec` is the codec the
/// frame was coded with, as its capture names it. `anchor` says a delta frame is a long-term
/// reference predicting from a frame every peer holds.
#[derive(Clone, Debug, Default)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub timestamp: u32,
    pub keyframe: bool,
    pub timing: Option<FrameTiming>,
    pub dependency: Option<(u16, Option<u16>)>,
    pub codec: Option<Codec>,
    pub anchor: bool,
}

/// One packet on its way to the wire, in the pieces the transport copies once into its
/// SRTP buffer: the RTP header and the payload's slices of the frame.
#[derive(Clone, Copy, Debug)]
pub struct Datagram<'a> {
    pub header: &'a [u8],
    pub payload: &'a [Bytes],
}

impl Datagram<'_> {
    pub fn len(&self) -> usize {
        self.header.len() + self.payload.iter().map(|p| p.len()).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.header);
        for p in self.payload {
            out.extend_from_slice(p);
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len());
        self.write_to(&mut out);
        out
    }
}

/// What a sender needs of its transport: the SRTP-protected send path with the pacer behind
/// it, the transport-wide sequence space, and when the peer was last heard from. The
/// transport implements it over its SRTP context, its `Pacer`, and its `TwccSender`.
pub trait SenderTransport {
    /// Protect and send one RTP packet, through the pacer where one paces; the pacer's
    /// verdict, `SendNow` where nothing paces.
    fn send_rtp(
        &mut self,
        packet: Datagram<'_>,
        class: Class,
        twcc_seq: Option<u16>,
        now: f64,
    ) -> Verdict;
    /// Protect and send one compound RTCP packet.
    fn send_rtcp(&mut self, compound: &[u8], now: f64);
    fn twcc(&mut self) -> &mut TwccSender;
    fn pacer(&mut self) -> Option<&mut Pacer>;
    /// When the peer was last heard from on the ICE path: media, DTLS, SCTP, or a consent check.
    fn peer_heard_at(&self) -> f64;

    /// The frames whose last packet waits in the pacer; how long a packet handed over now waits
    /// to reach the peer: what the pacer holds takes to leave, and the queue the newest
    /// transport-cc feedback found standing on the path; and the seconds since a feedback last
    /// reported a packet the wire lost.
    fn video_backlog(&mut self, now: f64) -> (usize, f64, f64) {
        let (frames, mut wait) = self
            .pacer()
            .map_or((0, 0.0), |p| (p.frames_pending(), p.drain_s()));
        let twcc = self.twcc();
        wait += twcc.path_queue_s();
        (frames, wait, now - twcc.lost_at())
    }

    /// Bytes of video the pacer queues before it abandons the GOP; `None` without a pacer.
    fn video_room(&mut self) -> Option<usize> {
        self.pacer().map(|p| p.video_room())
    }

    /// Seconds until a packet handed over now is on the wire.
    fn send_delay(&mut self) -> f64 {
        self.pacer().map_or(0.0, |p| p.drain_s())
    }

    /// Whether a padding packet of `size` bytes before protection goes out now; never without
    /// a pacer to admit it.
    fn padding_room(&mut self, _size: usize, _now: f64) -> bool {
        false
    }

    /// Protect and send one padding packet `padding_room` admitted, on the pacer's credit.
    fn send_padding(&mut self, _packet: Datagram<'_>, _twcc_seq: u16, _now: f64) {}
}

/// What the engine learns from a sender and answers it. Every method has a default that
/// does nothing or declines, so a sender for the display's owner needs none of them.
pub trait SenderHooks {
    /// A frame's last packet is on the wire, as `(capture_ns, payload bytes)`.
    fn on_frame_sent(&mut self, _captured_ns: i64, _size: usize) {}
    /// A frame was handed to the pacer; frames the pacer drops instead call nothing.
    fn on_frame_queued(&mut self) {}
    /// Whether this peer is served selectively, as one not owning its display; `None` where
    /// the engine never says, which serves it every frame.
    fn selective(&self) -> Option<bool> {
        None
    }
    /// Told a run's first frame is lost and whether the run is within the encoder's reach;
    /// whether the encoder took it. `None` where unwired, which raises `lost_frame`.
    fn on_resync(&mut self, _frame_id: u16, _reach: bool) -> Option<bool> {
        None
    }
    /// A frame was left out: neither queued nor sent.
    fn on_frame_left_out(&mut self) {}
    /// A described frame's last packet left for the peer.
    fn on_frame_out(&mut self, _frame_id: u16, _keyframe: bool) {}

    /// A frame's last packet left the pacer, for a measurement of the send path. `packets`
    /// counts the media and FlexFEC datagrams built for that frame.
    fn on_frame_left(
        &mut self,
        _timing: Option<FrameTiming>,
        _size: usize,
        _packets: u32,
        _keyframe: bool,
    ) {
    }
    /// Transport-cc reported each of a frame's packets received and the frame it predicts
    /// from is held.
    fn on_frame_held(&mut self, _frame_id: u16, _keyframe: bool) {}
    fn on_key_sent(&mut self) {}
    /// The peer lost `frame_id` past repair: the encoder takes it out of its references.
    fn lost_frame(&mut self, _frame_id: u16) {}
    /// The peer needs a key frame.
    fn pli(&mut self) {}
}

/// Hooks that do nothing.
pub struct NoHooks;

impl SenderHooks for NoHooks {}

/// The kind of media a sender carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Video,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Audio => "audio",
            Kind::Video => "video",
        }
    }
}

/// `start` found no media codec to send.
#[derive(Debug, thiserror::Error)]
#[error("No sendable media codec was negotiated")]
pub struct NoCodec;

/// A map that iterates in insertion order and evicts its oldest entries.
struct Ordered<K, V> {
    map: HashMap<K, (V, u64)>,
    order: VecDeque<(K, u64)>,
    generation: u64,
}

impl<K: std::hash::Hash + Eq + Copy, V> Ordered<K, V> {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            generation: 0,
        }
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key).map(|(v, _)| v)
    }

    fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Insert at the end; an existing key moves to the end.
    fn insert(&mut self, key: K, value: V) {
        self.generation += 1;
        self.map.insert(key, (value, self.generation));
        self.order.push_back((key, self.generation));
        self.compact();
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let (v, _) = self.map.remove(key)?;
        self.compact();
        Some(v)
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    /// Drop dead order entries when they outnumber the live ones.
    fn compact(&mut self) {
        if self.order.len() > 2 * self.map.len() + 64 {
            let map = &self.map;
            self.order
                .retain(|(k, g)| map.get(k).is_some_and(|(_, live)| live == g));
        }
    }

    fn pop_oldest(&mut self) -> Option<K> {
        while let Some((k, g)) = self.order.pop_front() {
            if self.map.get(&k).is_some_and(|(_, live)| *live == g) {
                self.map.remove(&k);
                return Some(k);
            }
        }
        None
    }

    /// Keys oldest first.
    fn keys(&self) -> impl DoubleEndedIterator<Item = K> + '_ {
        self.order
            .iter()
            .filter(|(k, g)| self.map.get(k).is_some_and(|(_, live)| live == g))
            .map(|(k, _)| *k)
    }

    fn iter(&self) -> impl Iterator<Item = (K, &V)> + '_ {
        self.keys().map(move |k| (k, &self.map[&k].0))
    }
}

/// A described frame sent and not yet held.
struct Unheld {
    frame_id: u16,
    reference: Option<u16>,
    keyframe: bool,
    sent_at: f64,
    /// `(transport-wide, media sequence number)` of each packet.
    packets: Vec<(u16, u16)>,
}

/// What `frame_left` reports for a frame's last packet.
struct FrameEnd {
    keyframe: bool,
    timing: Option<FrameTiming>,
    captured_ns: i64,
    size: usize,
    packets: u32,
    frame_id: Option<u16>,
}

/// A packet built for one frame, awaiting its send.
struct Built {
    header: Vec<u8>,
    payload: Payload,
    twcc_seq: Option<u16>,
    media_seq: Option<u16>,
    size: usize,
}

pub struct RtpSender {
    kind: Kind,
    hooks: Box<dyn SenderHooks + Send>,
    ssrc: u32,
    rtx_ssrc: u32,
    fec_ssrc: u32,
    enabled: bool,
    peer_silent: bool,
    packetizer: Option<Packetizer>,
    red_distance: Option<usize>,
    codecs: Vec<CodecParameters>,
    send_codec: Option<CodecParameters>,
    send_codec_kind: Option<Codec>,
    pending_codec: Option<String>,
    pub keyframe_bytes: Option<usize>,
    in_flight: VecDeque<(u16, u16)>,
    skips: Option<h264_skip::SkipPictures>,
    pub stand_ins: u64,
    resync_since: Option<f64>,
    resync_first: Option<u16>,
    resync_run: usize,
    resync_told: bool,
    /// This run already took one guarded repair window without resuming.
    repair_deferred: bool,
    since_key: usize,
    resyncs: u64,
    anchored: bool,
    unheld: VecDeque<Unheld>,
    stall_since: Option<f64>,
    stall_last: f64,
    held_at: f64,
    key_sent_at: Option<f64>,
    held: Ordered<u16, ()>,
    unheld_gap: bool,
    rtx_twcc: Ordered<u16, u16>,
    mid: Option<String>,
    cname: Option<String>,
    map: HeaderExtensionsMap,
    history: RtpHistory,
    frame_number: u16,
    frame_numbers: Ordered<u16, u16>,
    frame_handed: Option<u16>,
    frame_left: Option<(u16, f64)>,
    key_asked_at: f64,
    lost_at: Option<f64>,
    rtx_payload_type: Option<u8>,
    rtx_sequence_number: u16,
    fec_payload_type: Option<u8>,
    fec_sequence_number: u16,
    /// Repair packets per FlexFEC group, with interleaved masks.
    pub fec_repair_packets: usize,
    fec_loss: f64,
    fec_clear_loss: f64,
    fec_clear: u32,
    fec_full: bool,
    fec_credit: usize,
    /// The repair budget (`set_repair_budget`): its rate (None: unbounded), the bytes it holds
    /// and when it was last filled; what the path delivers; the key frame last asked for and the
    /// one last on the wire, as its last packet's transport-wide number, when it left, and its size.
    repair_bps: Option<f64>,
    repair_bytes: f64,
    repair_at: f64,
    delivery_bps: Option<f64>,
    key_requested: Option<f64>,
    /// Request history must roll only once, the first actual H264 IDR that answers it.
    response_history_pending: bool,
    key_wire: Option<(u16, f64, usize)>,
    started: bool,
    sequence_number: u16,
    timestamp_origin: u32,
    last_video_timing: f64,
    last_abs_capture_ns: i64,
    fec_group: Vec<(Vec<u8>, Payload)>,
    fec_first_seq: u16,
    frame_ends: VecDeque<(u16, FrameEnd)>,
    lsr: Option<u32>,
    lsr_time: f64,
    rtp_timestamp: u32,
    rtp_instant_ns: i64,
    rtp_clock_rate: u32,
    octet_count: u64,
    packet_count: u64,
    rtt: Option<f64>,
    rrtrs: Ordered<u32, (u32, i64)>,
    header_buf: Vec<u8>,
    /// The packets padding resends (`send_padding`), and which goes next.
    padding_sources: VecDeque<RtpPacket>,
    padding_next: usize,
}

impl RtpSender {
    pub fn new(kind: Kind, hooks: Box<dyn SenderHooks + Send>) -> Self {
        Self {
            kind,
            hooks,
            ssrc: random32(),
            rtx_ssrc: random32(),
            fec_ssrc: random32(),
            enabled: true,
            peer_silent: false,
            packetizer: None,
            red_distance: None,
            codecs: Vec::new(),
            send_codec: None,
            send_codec_kind: None,
            pending_codec: None,
            keyframe_bytes: None,
            in_flight: VecDeque::with_capacity(IN_FLIGHT_MAX),
            skips: None,
            stand_ins: 0,
            resync_since: None,
            resync_first: None,
            resync_run: 0,
            resync_told: false,
            repair_deferred: false,
            since_key: 0,
            resyncs: 0,
            anchored: false,
            unheld: VecDeque::with_capacity(UNHELD_MAX),
            stall_since: None,
            stall_last: 0.0,
            held_at: 0.0,
            key_sent_at: None,
            held: Ordered::new(),
            unheld_gap: false,
            rtx_twcc: Ordered::new(),
            mid: None,
            cname: None,
            map: HeaderExtensionsMap::new(),
            history: RtpHistory::new(),
            frame_number: 0,
            frame_numbers: Ordered::new(),
            frame_handed: None,
            frame_left: None,
            key_asked_at: 0.0,
            lost_at: None,
            rtx_payload_type: None,
            rtx_sequence_number: random_sequence_number(),
            fec_payload_type: None,
            fec_sequence_number: random_sequence_number(),
            fec_repair_packets: 1,
            fec_loss: 0.0,
            fec_clear_loss: 0.0,
            fec_clear: 0,
            fec_full: false,
            fec_credit: 0,
            repair_bps: None,
            repair_bytes: 0.0,
            repair_at: 0.0,
            delivery_bps: None,
            key_requested: None,
            response_history_pending: false,
            key_wire: None,
            started: false,
            sequence_number: random_sequence_number(),
            timestamp_origin: random32(),
            last_video_timing: 0.0,
            last_abs_capture_ns: 0,
            fec_group: Vec::with_capacity(FEC_GROUP_PACKETS),
            fec_first_seq: 0,
            frame_ends: VecDeque::with_capacity(FRAME_ENDS_MAX),
            lsr: None,
            lsr_time: 0.0,
            rtp_timestamp: 0,
            rtp_instant_ns: 0,
            rtp_clock_rate: 0,
            octet_count: 0,
            packet_count: 0,
            rtt: None,
            rrtrs: Ordered::new(),
            header_buf: Vec::with_capacity(96),
            padding_sources: VecDeque::with_capacity(PADDING_SOURCES),
            padding_next: 0,
        }
    }

    /// Keep a video packet large enough to pad with, the newest `PADDING_SOURCES` alone.
    fn keep_padding_source(&mut self, packet: &RtpPacket) {
        if self.kind != Kind::Video || packet.payload.len() < PADDING_SOURCE_BYTES {
            return;
        }
        if self.padding_sources.len() == PADDING_SOURCES {
            self.padding_sources.pop_front();
        }
        self.padding_sources.push_back(packet.clone());
    }

    /// Resend the stream's last packets on its RTX stream, up to `budget` bytes, as the padding
    /// a still screen's path probe measures the path with (the module docstring); the bytes
    /// sent. Nothing pads without RTX negotiated, a packet to resend, or the sender enabled.
    pub fn send_padding(
        &mut self,
        budget: usize,
        transport: &mut dyn SenderTransport,
        now: f64,
    ) -> usize {
        let Some(pt) = self.rtx_payload_type else {
            return 0;
        };
        if self.kind != Kind::Video || !self.enabled || self.padding_sources.is_empty() {
            return 0;
        }
        let mut sent = 0;
        while sent < budget {
            let original = &self.padding_sources[self.padding_next % self.padding_sources.len()];
            let size = original.payload.len() + 2;
            if !transport.padding_room(size + PADDING_HEADER_BYTES, now) {
                break;
            }
            self.padding_next = self.padding_next.wrapping_add(1);
            let mut packet = wrap_rtx(original, pt, self.rtx_sequence_number, self.rtx_ssrc);
            self.rtx_sequence_number = self.rtx_sequence_number.wrapping_add(1);
            let seq = transport.twcc().next_padding(packet.payload.len(), now);
            packet.extensions.transport_sequence_number = Some(seq);
            let mut header = std::mem::take(&mut self.header_buf);
            header.clear();
            packet.write_header(&self.map, &mut header);
            let datagram = Datagram {
                header: &header,
                payload: packet.payload.pieces(),
            };
            sent += datagram.len();
            transport.send_padding(datagram, seq, now);
            self.header_buf = header;
        }
        sent
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub fn rtx_ssrc(&self) -> u32 {
        self.rtx_ssrc
    }

    pub fn fec_ssrc(&self) -> u32 {
        self.fec_ssrc
    }

    /// Give the sender fixed SSRCs, as the SDP that announced them requires.
    pub fn set_ssrcs(&mut self, ssrc: u32, rtx_ssrc: u32, fec_ssrc: u32) {
        self.ssrc = ssrc;
        self.rtx_ssrc = rtx_ssrc;
        self.fec_ssrc = fec_ssrc;
    }

    /// The frames of redundancy a RED stream carries.
    pub fn set_red_distance(&mut self, distance: Option<usize>) {
        self.red_distance = distance;
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// The prior payloads this sender's RED packets carry and the configured depth, or None
    /// where it sends no RED.
    pub fn red_depth(&mut self) -> Option<(usize, usize)> {
        self.packetizer.as_mut()?.red().map(|r| r.depth())
    }

    /// Carry `distance` prior payloads as RED redundancy from the next packet on, or the
    /// configured depth (None), where this sender sends RED (`RedState::set_distance`).
    pub fn shed_red_depth(&mut self, distance: Option<usize>) {
        if let Some(red) = self.packetizer.as_mut().and_then(Packetizer::red) {
            red.set_distance(distance);
        }
    }

    pub fn packet_count(&self) -> u64 {
        self.packet_count
    }

    pub fn octet_count(&self) -> u64 {
        self.octet_count
    }

    pub fn rtt(&self) -> Option<f64> {
        self.rtt
    }

    pub fn header_extensions(&self) -> &HeaderExtensionsMap {
        &self.map
    }

    /// Start sending with the negotiated parameters: the first codec with a packetizer
    /// (auxiliary entries, RTX and FlexFEC, can top the list when the media codec was
    /// filtered out), its RTX payload type, and the FlexFEC payload type where negotiated.
    pub fn start(&mut self, parameters: &SendParameters) -> Result<(), NoCodec> {
        if self.started {
            return Ok(());
        }
        self.cname = parameters.cname.clone();
        self.mid = parameters.mux_id.clone();
        self.map.configure(&parameters.header_extensions);
        let mut send_codec = parameters
            .codecs
            .iter()
            .find(|c| !c.is_rtx() && !c.is_flexfec())
            .cloned()
            .ok_or(NoCodec)?;
        self.codecs = parameters.codecs.clone();
        if let Some(pending) = self.pending_codec.take()
            && let Some(codec) = self.negotiated_codec(&pending)
        {
            send_codec = codec;
        }
        self.rtx_payload_type = self.rtx_payload_type_for(&send_codec);
        self.send_codec_kind = Codec::from_mime(&send_codec.mime_type);
        self.send_codec = Some(send_codec);
        self.fec_payload_type = self
            .codecs
            .iter()
            .find(|c| c.is_flexfec())
            .map(|c| c.payload_type);
        self.started = true;
        Ok(())
    }

    fn rtx_payload_type_for(&self, codec: &CodecParameters) -> Option<u8> {
        self.codecs
            .iter()
            .find(|c| c.is_rtx() && c.apt() == Some(codec.payload_type))
            .map(|c| c.payload_type)
    }

    /// The negotiated media codec of `mime_type`, or `None` when the peer did not take it.
    pub fn negotiated_codec(&self, mime_type: &str) -> Option<CodecParameters> {
        let wanted = mime_type.to_ascii_lowercase();
        self.codecs
            .iter()
            .find(|c| c.mime_lower() == wanted)
            .cloned()
    }

    /// Send the track's frames as the negotiated codec of `mime_type` from now on: its
    /// payload type, its RTX type, and its own packer. A codec the peer never took drops the
    /// frames instead, until a switch names one it did; before the sender starts, the switch
    /// names the codec it starts on.
    pub fn switch_codec(&mut self, mime_type: &str) -> bool {
        if !self.started {
            self.pending_codec = Some(mime_type.to_string());
            return true;
        }
        let codec = self.negotiated_codec(mime_type);
        self.rtx_payload_type = codec.as_ref().and_then(|c| self.rtx_payload_type_for(c));
        self.send_codec_kind = codec.as_ref().and_then(|c| Codec::from_mime(&c.mime_type));
        let taken = codec.is_some();
        self.send_codec = codec;
        self.packetizer = None;
        self.anchored = false;
        taken
    }

    pub fn fec_enabled(&self) -> bool {
        self.fec_payload_type.is_some()
    }

    /// Set the FlexFEC repair density from a measured loss fraction: one repair per group
    /// under 2% loss, two under 8%, three above, read on a smoothed loss so a single small
    /// window neither adds nor drops a repair on its own. Every group takes them while the
    /// path loses packets with no queue standing (`queued`), a loss a repair recovers without
    /// a retransmission's round trip: the loss read on windows `FEC_CLEAR_WINDOWS` past the
    /// last queue, since a queue's overflow shows in the windows it stood in and the ones
    /// just after. With nothing lost that way, or while a queue stands, the repairs follow the
    /// media packets (`fec_repairs`).
    pub fn steer_fec(&mut self, loss_fraction: f64, queued: bool) {
        self.fec_loss += (loss_fraction - self.fec_loss) * 0.3;
        self.fec_clear = if queued { 0 } else { self.fec_clear + 1 };
        if self.fec_clear > FEC_CLEAR_WINDOWS {
            self.fec_clear_loss += (loss_fraction - self.fec_clear_loss) * 0.3;
        }
        self.fec_repair_packets =
            1 + (self.fec_loss > 0.02) as usize + (self.fec_loss > 0.08) as usize;
        self.fec_full = self.fec_clear_loss > FEC_FULL_LOSS && !queued;
    }

    /// How many repairs a FlexFEC group of `packets` media packets gets: `fec_repair_packets`,
    /// never more than it has packets, where `steer_fec` wants every group repaired.
    /// Otherwise `fec_repair_packets` per `FEC_GROUP_PACKETS` media packets, what a group
    /// falls short of carried to the next: a group closes at every frame, so a low-rate
    /// stream's frames of one or two packets would each take a repair and double what the
    /// path carries.
    pub fn fec_repairs(&mut self, packets: usize) -> usize {
        if self.fec_full {
            self.fec_credit = 0;
            return self.fec_repair_packets.min(packets);
        }
        self.fec_credit += packets * self.fec_repair_packets;
        let repairs = packets.min(self.fec_credit / FEC_GROUP_PACKETS);
        self.fec_credit -= repairs * FEC_GROUP_PACKETS;
        repairs
    }

    fn numbered(&self) -> bool {
        self.send_codec_kind == Some(Codec::H264)
    }

    /// Whether frames naming their reference are numbered for this peer and left out where
    /// it has no room: one reading the dependency descriptor, or an H.264 one without it
    /// (Firefox answers so), which finds a frame's reference by sequence number, unbroken by
    /// a frame left out before it is packed; its frames carry no descriptor.
    fn numbers_frames(&self) -> bool {
        self.map.has_dependency_descriptor() || self.numbered()
    }

    fn far(&self, transport: &mut dyn SenderTransport, now: f64) -> bool {
        let (frames, wait, lost) = transport.video_backlog(now);
        frames >= RESYNC_FAR_FRAMES
            || wait >= RESYNC_FAR_S
            || (lost < RESYNC_LOSS_S && wait >= RESYNC_LAG_S)
    }

    fn anchor_room(&self, transport: &mut dyn SenderTransport, now: f64) -> bool {
        !self.far(transport, now) && transport.video_backlog(now).1 < RESYNC_ANCHOR_S
    }

    fn open_run(&mut self, frame_id: u16, now: f64) {
        self.resync_since = Some(now);
        self.resync_first = Some(frame_id);
        self.resync_run = 1;
        self.resync_told = false;
        self.repair_deferred = false;
    }

    fn close_run(&mut self) {
        self.resync_since = None;
        self.resync_first = None;
        self.resync_run = 0;
        self.resync_told = false;
        self.repair_deferred = false;
    }

    /// A frame of the open run was held back: tell the encoder of the run's first frame once
    /// the peer has room, or sooner where waiting would cost a key frame
    /// (`RESYNC_REACH_FRAMES`). Past `RESYNC_S` (`RESYNC_ANCHORED_S` where anchors run), a
    /// held resync or a key already due to this peer answers instead of another encoder
    /// request. A resync that only keeps the same run waiting gets one such window; its next
    /// deadline asks for a key frame rather than deferring forever. The run stays open after
    /// that request, so deltas still predicting from it cannot redrive PLI before the key.
    fn repair(&mut self, transport: &mut dyn SenderTransport, now: f64) {
        let Some(since) = self.resync_since else {
            return;
        };
        let limit = if self.anchored {
            RESYNC_ANCHORED_S
        } else {
            RESYNC_S
        };
        if now - since > limit {
            let deferred = self.repair_deferred;
            if self.key_on_its_way(transport, now) {
                return;
            }
            if !deferred && self.queued_key_on_its_way(transport, now) {
                self.resync_since = Some(now);
                self.repair_deferred = true;
                return;
            }
            let first = self.resync_first.expect("an open run has a first frame");
            let run = self.resync_run;
            let told = self.resync_told;
            self.close_run();
            if !deferred && self.resync_held(transport, now) {
                if self.resync_since.is_none() {
                    self.open_run(first, now);
                    self.resync_run = run;
                    self.resync_told = told;
                    self.repair_deferred = true;
                }
                return;
            }
            log::info!(
                target: "webrtc",
                "{} sender {}: frame {:?} was never predicted past; asking for a key frame",
                self.kind.as_str(), self.ssrc, Some(first)
            );
            self.ask_key_frame(transport, now);
            self.open_run(first, now);
            self.resync_run = run;
            self.resync_told = told;
            self.repair_deferred = true;
            return;
        }
        if self.resync_told {
            return;
        }
        let (frames, wait, _) = transport.video_backlog(now);
        let due = self.resync_run >= RESYNC_REACH_FRAMES
            || (self.numbered()
                && (FRAME_NUM_WRAP - self.since_key % FRAME_NUM_WRAP) % FRAME_NUM_WRAP
                    <= RESYNC_WRAP_LEAD);
        if (frames < RESYNC_ROOM_FRAMES && wait < RESYNC_LAG_S)
            || (due && !self.far(transport, now))
        {
            // Where anchors run, a run of any depth is predicted past from the one pinned.
            let reach = self.resync_run <= RESYNC_REACH_FRAMES || self.anchored;
            self.lost_at = Some(now);
            let first = self.resync_first.expect("an open run has a first frame");
            match self.hooks.on_resync(first, reach) {
                Some(taken) => self.resync_told = taken,
                None => {
                    self.hooks.lost_frame(first);
                    self.resync_told = true;
                }
            }
        }
    }

    /// Whether a frame naming the frame it predicts from goes to this peer; false for one
    /// left out, or held back for predicting from one. The first frame predicting past an
    /// open run, which decodes on the peer, ends it, and an anchor goes without room unless
    /// the peer is far behind.
    fn forward(
        &mut self,
        frame_id: u16,
        reference: Option<u16>,
        keyframe: bool,
        anchor: bool,
        transport: &mut dyn SenderTransport,
        now: f64,
    ) -> bool {
        if keyframe {
            self.since_key = 0;
            self.close_run();
            return true;
        }
        self.since_key += 1;
        self.anchored = self.anchored || anchor;
        let sent = reference.is_none_or(|r| self.frame_numbers.contains(&r));
        // In H.264 a frame where frame_num may wrap is never left out.
        let kept = self.numbered() && self.since_key.is_multiple_of(FRAME_NUM_WRAP);
        if self.resync_since.is_some() {
            if sent && (!anchor || kept || self.anchor_room(transport, now)) {
                self.close_run();
                return true;
            }
            if sent {
                // An anchor left out predicts past the run: the encoder is told of it.
                self.resync_first = Some(frame_id);
                self.resync_told = false;
            }
            self.resync_run += 1;
            self.repair(transport, now);
            return false;
        }
        let selective = self.hooks.selective();
        if !sent || selective.is_none() || kept {
            return true;
        }
        if anchor && self.anchor_room(transport, now) {
            return true;
        }
        let (frames, wait, _) = transport.video_backlog(now);
        if (frames < RESYNC_ROOM_FRAMES && wait < RESYNC_LAG_S) || selective != Some(true) {
            return true;
        }
        self.open_run(frame_id, now);
        self.repair(transport, now);
        false
    }

    /// Number a frame on the wire and measure how far back it predicts, for its dependency
    /// descriptor: `(number, frames back)`, the latter `None` for a frame that predicts from
    /// nothing. `None` for a frame predicting from one this sender never sent (a peer paused
    /// across it): undecodable here, so the caller leaves it out.
    fn describe(
        &mut self,
        frame_id: u16,
        reference: Option<u16>,
        keyframe: bool,
    ) -> Option<(u16, Option<u16>)> {
        if keyframe {
            self.frame_numbers.clear();
        }
        let mut fdiff = None;
        if let Some(reference) = reference {
            let known = *self.frame_numbers.get(&reference)?;
            let diff = self.frame_number.wrapping_sub(known);
            if !(1..=4096).contains(&diff) {
                return None;
            }
            fdiff = Some(diff);
        }
        let number = self.frame_number;
        self.frame_number = number.wrapping_add(1);
        self.frame_numbers.insert(frame_id, number);
        while self.frame_numbers.len() > FRAME_NUMBERS_MEMORY {
            self.frame_numbers.pop_oldest();
        }
        Some((number, fdiff))
    }

    /// A frame predicting from one this peer was never sent: nothing but a key frame decodes
    /// here, and one is asked for.
    fn undecodable(&mut self, frame_id: u16, reference: Option<u16>) {
        log::debug!(
            target: "webrtc",
            "{} sender {}: frame {} predicts from {:?}, which this peer was not sent; asking for a key frame",
            self.kind.as_str(), self.ssrc, frame_id, reference
        );
        self.hooks.pli();
    }

    /// A pacer GOP reset, told the oldest packet it dropped. Where the encoder names each
    /// frame's reference, the peer reads the dependency descriptor and does not own its
    /// display, the frames whose last packet had not left are lost to the peer: they leave
    /// the frames it was sent, so `forward` holds back whatever predicts from them, NACKs for
    /// the dropped packets go unanswered, and the run is answered as a run left out is. False
    /// where this sender cannot, which leaves the pacer its key frame.
    fn pacer_resync(
        &mut self,
        first_tag: Option<u16>,
        transport: &mut dyn SenderTransport,
        now: f64,
    ) -> bool {
        // Whatever key frame was on its way, the reset may have dropped: the next request asks again.
        self.key_requested = None;
        self.response_history_pending = false;
        self.key_wire = None;
        self.key_sent_at = None;
        let Some(first_tag) = first_tag else {
            return false;
        };
        if self.in_flight.is_empty()
            || self.hooks.selective() != Some(true)
            || !self.map.has_dependency_descriptor()
        {
            return false;
        }
        let lost: Vec<u16> = self
            .in_flight
            .iter()
            .filter(|(tag, _)| tag.wrapping_sub(first_tag) < 0x8000)
            .map(|(_, fid)| *fid)
            .collect();
        if lost.is_empty() {
            return false;
        }
        for fid in &lost {
            self.frame_numbers.remove(fid);
        }
        self.in_flight.clear();
        self.history.abandon();
        self.resyncs += 1;
        if self.resync_since.is_none() {
            self.open_run(lost[0], now);
            self.resync_run = lost.len();
        } else {
            self.resync_run += lost.len();
        }
        self.repair(transport, now);
        true
    }

    /// Tell `on_frame_held` of each frame sent the peer now holds: every packet of it
    /// reported received, or the retransmission of one, and the frame it predicts from held.
    /// A frame not held within `HELD_S` never will be. A peer without the dependency
    /// descriptor decodes nothing past a frame it lacks until a key frame, so it holds a
    /// frame only once it holds every frame sent before it.
    fn confirm(&mut self, transport: &mut dyn SenderTransport, now: f64) {
        let in_order = !self.map.has_dependency_descriptor();
        let mut kept: VecDeque<Unheld> = VecDeque::with_capacity(self.unheld.len());
        let unheld = std::mem::take(&mut self.unheld);
        let twcc = transport.twcc();
        for entry in unheld {
            if now - entry.sent_at > HELD_S {
                self.unheld_gap = self.unheld_gap || in_order;
                continue;
            }
            let acked = entry.packets.iter().all(|(seq, media)| {
                twcc.arrived(*seq)
                    || self
                        .rtx_twcc
                        .get(media)
                        .is_some_and(|rtx| twcc.arrived(*rtx))
            });
            if !(in_order && (!kept.is_empty() || self.unheld_gap))
                && acked
                && (entry.keyframe || entry.reference.is_some_and(|r| self.held.contains(&r)))
            {
                self.held.remove(&entry.frame_id);
                self.held.insert(entry.frame_id, ());
                self.held_at = now;
                while self.held.len() > FRAME_NUMBERS_MEMORY {
                    self.held.pop_oldest();
                }
                self.hooks.on_frame_held(entry.frame_id, entry.keyframe);
            } else {
                kept.push_back(entry);
            }
        }
        self.unheld = kept;
        if self.rtx_twcc.len() > 1024 {
            for _ in 0..512 {
                self.rtx_twcc.pop_oldest();
            }
        }
    }

    /// Whether a NACK from a peer that does not own its display, and reads the dependency
    /// descriptor, names more than its pacer queue holds, a first repair going out twice:
    /// the burst a link that went dark asks for once it is back, whose retransmissions would
    /// overflow the queue and cost every peer a key frame. It is answered as a loss past
    /// repair.
    fn past_repair(&mut self, lost: &[u16], transport: &mut dyn SenderTransport) -> bool {
        if self.kind != Kind::Video
            || self.hooks.selective() != Some(true)
            || !self.map.has_dependency_descriptor()
        {
            return false;
        }
        let Some(room) = transport.video_room() else {
            return false;
        };
        let held: usize = lost
            .iter()
            .filter_map(|seq| self.history.get(*seq))
            .map(|p| p.payload.len())
            .sum();
        2 * held > room
    }

    /// Answer a peer that lost frames past repair as a run the pacer cut is, where it does
    /// not own its display and anchors run: the frames sent it after the newest it holds
    /// leave the frames it was sent, and the encoder is told the first is lost, so a frame
    /// predicting from one it holds comes next. A request while that run is open, or while
    /// the key frame it was sent is on its way, is answered by them. False where it cannot,
    /// which asks for a key frame: past `RESYNC_ANCHORED_S` of such requests one after
    /// another while it holds frames it is sent (a decoder that only takes a key frame; one
    /// holding nothing new between two requests is starved and starts the count again),
    /// where it holds nothing and no key frame is due, and for a peer without the descriptor.
    /// A peer whose link is dark (`RESYNC_DARK_S`) is answered by its request once it is
    /// back: neither a resync nor a key frame reaches it now.
    fn resync_held(&mut self, transport: &mut dyn SenderTransport, now: f64) -> bool {
        if self.hooks.selective() != Some(true)
            || !self.anchored
            || !self.map.has_dependency_descriptor()
        {
            return false;
        }
        let (received_at, sent_at) = {
            let twcc = transport.twcc();
            (twcc.received_at(), twcc.sent_at())
        };
        if 0.0 < received_at && received_at < now - RESYNC_DARK_S && now - RESYNC_DARK_S < sent_at {
            return true;
        }
        if self.stall_since.is_none()
            || now - self.stall_last > RESYNC_ANCHORED_S
            || self.held_at < self.stall_last
        {
            self.stall_since = Some(now);
        }
        self.stall_last = now;
        if now - self.stall_since.unwrap_or(now) > RESYNC_ANCHORED_S {
            self.stall_since = None;
            return false;
        }
        if self.resync_since.is_some() {
            self.repair(transport, now);
            return true;
        }
        if self.held.is_empty() {
            return self.queued_key_on_its_way(transport, now);
        }
        let Some(number) = self
            .held
            .keys()
            .rev()
            .find_map(|fid| self.frame_numbers.get(&fid).copied())
        else {
            return false;
        };
        let after: Vec<u16> = self
            .frame_numbers
            .iter()
            .filter(|(_, n)| {
                let d = n.wrapping_sub(number);
                0 < d && d < 0x8000
            })
            .map(|(fid, _)| fid)
            .collect();
        if after.is_empty() {
            return true;
        }
        for fid in &after {
            self.frame_numbers.remove(fid);
        }
        // The encoder is told the newest of them predicting from none of the others: it
        // predicted past the frames before that one already.
        let refs: HashMap<u16, Option<u16>> = self
            .unheld
            .iter()
            .map(|u| (u.frame_id, u.reference))
            .collect();
        let first = after
            .iter()
            .rev()
            .find(|fid| {
                refs.get(fid)
                    .is_some_and(|r| !r.is_some_and(|r| after.contains(&r)))
            })
            .copied()
            .unwrap_or(after[0]);
        self.open_run(first, now);
        self.resync_run = after.len();
        self.repair(transport, now);
        true
    }

    /// Whether a key-frame request is a receiver's wait on a still screen
    /// (`STILL_REQUEST_S`), which a key frame would only restart: the newest frame is on the
    /// wire and acknowledged, it left `STILL_REQUEST_S` and the round trip ago, no frame lost
    /// past repair awaits one predicting past it, and the request before came
    /// `STILL_REQUEST_S` or more earlier.
    fn still_request(&mut self, transport: &mut dyn SenderTransport, now: f64) -> bool {
        let before = self.key_asked_at;
        self.key_asked_at = now;
        let Some((seq, left_at)) = self.frame_left else {
            return false;
        };
        Some(seq) == self.frame_handed
            && self.lost_at.is_none()
            && now - left_at >= STILL_REQUEST_S + self.rtt.unwrap_or(0.0)
            && now - before >= STILL_REQUEST_S
            && transport.twcc().acked(seq)
    }

    /// A frame's last packet left for the wire at `at`: the pacer's `FrameLeft` for a tag
    /// `frame_end` registered. One whose encode began after the peer last lost a frame
    /// predicts past it, as a key frame predicts from nothing.
    /// Bound repairs to `REPAIR_SHARE` of `bps`, the rate congestion control steered the display
    /// to (None: no bound), and time a key frame's way by `delivery_bps`, what the path delivers
    /// (`key_on_its_way`).
    pub fn set_repair_budget(&mut self, bps: Option<f64>, delivery_bps: Option<f64>) {
        self.repair_bps = bps.map(|b| b * REPAIR_SHARE);
        self.delivery_bps = delivery_bps;
    }

    /// Whether a repair of `size` bytes fits the budget, which it then takes. A `proactive` one
    /// (FlexFEC) leaves `FEC_RESERVE` of the budget to the retransmissions a NACK asks for, which
    /// answer a loss that happened.
    fn repair_room(&mut self, size: usize, proactive: bool, now: f64) -> bool {
        let Some(bps) = self.repair_bps.filter(|b| *b > 0.0) else {
            return true;
        };
        let full = bps / 8.0 * REPAIR_WINDOW_S;
        self.repair_bytes = full.min(self.repair_bytes + (now - self.repair_at) * bps / 8.0);
        self.repair_at = now;
        let reserve = if proactive { full * FEC_RESERVE } else { 0.0 };
        if self.repair_bytes - reserve < size as f64 {
            return false;
        }
        self.repair_bytes -= size as f64;
        true
    }

    /// Whether this selective peer's newest key frame is still due to reach it. Packetization
    /// clears the peer's held references then, even where its key waits in the pacer; two queue
    /// waits and round trips, with `RESYNC_S` as a floor, bound that wait. No frame held since the
    /// key means it is the peer's only available sync point.
    fn queued_key_on_its_way(&mut self, transport: &mut dyn SenderTransport, now: f64) -> bool {
        if self.hooks.selective() != Some(true) || !self.held.is_empty() {
            return false;
        }
        let wait = transport.video_backlog(now).1;
        self.key_sent_at
            .is_some_and(|at| now - at < RESYNC_S.max(2.0 * (wait + self.rtt.unwrap_or(0.0))))
    }

    /// Whether a key frame answers a request already, where congestion control measures what the
    /// path delivers (`set_repair_budget`): one asked of the encoder `KEY_ASK_S` ago or less, or one
    /// on the wire whose last packet is not acknowledged and which has had less than the round
    /// trip, `KEY_MARGIN_S` and its bytes' time at what the path delivers. A NACK naming a packet
    /// gone from the history, which every loss on a full path does, otherwise asked for a key frame
    /// of its own: four a second, each overflowing the path again; a peer's PLI repeats while the
    /// key frame is still crossing a slow path. A pacer reset drops what was on its way
    /// (`pacer_resync`).
    fn key_on_its_way(&mut self, transport: &mut dyn SenderTransport, now: f64) -> bool {
        let Some(delivery) = self.delivery_bps.filter(|d| *d > 0.0) else {
            return false;
        };
        if self.key_requested.is_some_and(|at| now - at < KEY_ASK_S) {
            return true;
        }
        let Some((seq, at, size)) = self.key_wire else {
            return false;
        };
        if transport.twcc().acked(seq) {
            return false;
        }
        let travel = self.rtt.unwrap_or(0.0)
            + KEY_MARGIN_S
            + size as f64 * 8.0 / delivery.max(KEY_FLOOR_BPS);
        now - at < travel
    }

    /// Ask the encoder for a key frame, which answers the requests after it. Its own stale queue
    /// cannot arrest this peer's recovery behind an IDR that is already on its way.
    fn ask_key_frame(&mut self, transport: &mut dyn SenderTransport, now: f64) {
        if self.kind == Kind::Video
            && let Some(pacer) = transport.pacer()
        {
            pacer.expect_keyframe(now);
        }
        self.key_requested = Some(now);
        self.response_history_pending |= self.kind == Kind::Video
            && transport
                .pacer()
                .is_some_and(|pacer| pacer.receiver_recovery());
        self.hooks.pli();
    }

    pub fn frame_left(&mut self, tag: u16, at: f64) {
        let Some(end) = self.forget_frame_end(tag) else {
            return;
        };
        if let Some(frame_id) = end.frame_id {
            self.hooks.on_frame_out(frame_id, end.keyframe);
        }
        self.hooks
            .on_frame_left(end.timing, end.size, end.packets, end.keyframe);
        self.frame_left = Some((tag, at));
        if end.keyframe {
            self.key_wire = Some((tag, at, end.size));
            self.key_requested = None;
        }
        while self
            .in_flight
            .front()
            .is_some_and(|(t, _)| tag.wrapping_sub(*t) < 0x8000)
        {
            self.in_flight.pop_front();
        }
        let encoded = end
            .timing
            .map(|t| t.encode_start_ns as f64 / 1e9)
            .filter(|e| *e > 0.0)
            .unwrap_or(0.0);
        if let Some(lost_at) = self.lost_at
            && (end.keyframe || encoded > lost_at)
        {
            self.lost_at = None;
        }
        self.hooks.on_frame_sent(end.captured_ns, end.size);
    }

    /// Apply the pacer's events after a call into it: dropped tags to the transport-cc
    /// accounting and the frames waiting on them, frames that left to this sender, a keyframe
    /// request to the hooks. The driver calls it with what `Pacer::drain` raised; the pacer
    /// serves one video sender, so an event is this sender's whichever call raised it.
    pub fn apply_pacer_events(&mut self, events: Vec<PacerEvent>, twcc: &mut TwccSender) {
        for event in events {
            match event {
                PacerEvent::Dropped(tag) => {
                    self.forget_frame_end(tag);
                    twcc.dropped(tag);
                }
                PacerEvent::FrameLeft { tag, at } => self.frame_left(tag, at),
                PacerEvent::KeyframeRequest => self.hooks.pli(),
            }
        }
    }

    fn take_pacer_events(&mut self, transport: &mut dyn SenderTransport) {
        // Audio bypasses the bucket and raises nothing; what is pending is the video sender's.
        if self.kind != Kind::Video {
            return;
        }
        let Some(events) = transport.pacer().map(|p| p.take_events()) else {
            return;
        };
        if !events.is_empty() {
            self.apply_pacer_events(events, transport.twcc());
        }
    }

    /// Hand a packet to the transport; false when the pacer dropped it. A video packet the
    /// pacer refuses was abandoned with its GOP, and every packet the history holds with it:
    /// NACKs for them go unanswered, since the keyframe the pacer asked for is their repair.
    fn send(
        &mut self,
        header: &[u8],
        payload: &Payload,
        twcc_seq: Option<u16>,
        transport: &mut dyn SenderTransport,
        now: f64,
    ) -> bool {
        let class = if self.kind == Kind::Audio {
            Class::AUDIO
        } else {
            Class::Video
        };
        let verdict = transport.send_rtp(
            Datagram {
                header,
                payload: payload.pieces(),
            },
            class,
            twcc_seq,
            now,
        );
        let preserve_history = matches!(&verdict, Verdict::Dropped)
            && transport
                .pacer()
                .is_some_and(|pacer| pacer.receiver_recovery());
        let sent = match verdict {
            Verdict::SendNow => {
                if transport.pacer().is_none()
                    && let Some(tag) = twcc_seq
                {
                    self.frame_left(tag, now);
                }
                true
            }
            Verdict::Queued => true,
            Verdict::Dropped => false,
            Verdict::Overflow { first_tag } => {
                let taken = self.pacer_resync(first_tag, transport, now);
                if let Some(pacer) = transport.pacer() {
                    pacer.resolve_overflow(taken, now);
                }
                false
            }
        };
        self.take_pacer_events(transport);
        if !sent && !preserve_history {
            self.history.abandon();
        }
        sent
    }

    /// Retransmit an RTP packet reported lost; false when the pacer dropped it.
    fn retransmit(
        &mut self,
        packet: RtpPacket,
        transport: &mut dyn SenderTransport,
        now: f64,
    ) -> bool {
        let media_seq = packet.sequence_number;
        let mut packet = match self.rtx_payload_type {
            Some(pt) => {
                let rtx = wrap_rtx(&packet, pt, self.rtx_sequence_number, self.rtx_ssrc);
                self.rtx_sequence_number = self.rtx_sequence_number.wrapping_add(1);
                rtx
            }
            None => packet,
        };
        // A retransmission is a new packet on the wire: give it its own transport-wide
        // sequence number.
        let seq = transport.twcc().next(packet.payload.len(), now);
        packet.extensions.transport_sequence_number = Some(seq);
        if !self.unheld.is_empty() {
            self.rtx_twcc.insert(media_seq, seq);
        }
        let mut header = std::mem::take(&mut self.header_buf);
        header.clear();
        packet.write_header(&self.map, &mut header);
        let sent = self.send(&header, &packet.payload, Some(seq), transport, now);
        self.header_buf = header;
        sent
    }

    /// Whether the peer has sent nothing at all for `PEER_SILENCE_S`, so frames are not sent
    /// to a client that left without a goodbye. Its first packet after the silence asks for a
    /// key frame: the frames skipped meanwhile broke the prediction chain.
    fn peer_gone(&mut self, transport: &mut dyn SenderTransport, now: f64) -> bool {
        let silent = now - transport.peer_heard_at() > PEER_SILENCE_S;
        if silent != self.peer_silent {
            self.peer_silent = silent;
            log::info!(
                target: "webrtc",
                "{} sender {}: the peer {}",
                self.kind.as_str(),
                self.ssrc,
                if silent { "went silent; sending stopped" } else { "is back; sending again" }
            );
            if !silent {
                self.hooks.pli();
            }
        }
        silent
    }

    /// Keep the SPS and PPS a key frame sent this peer carries, which its stand-ins' slice
    /// headers are read against.
    fn learn_parameter_sets(&mut self, data: &Bytes) {
        let nals = h264::split_bitstream(data);
        self.skips
            .get_or_insert_with(h264_skip::SkipPictures::new)
            .learn(nals.iter().map(|n| &n[..]));
    }

    /// A picture of skipped macroblocks in place of an H.264 frame left out for a peer
    /// without the dependency descriptor, under the frame's own frame_num and reference
    /// marking: its receiver hands the decoder every frame it is sent by sequence number, and
    /// a decoder that refuses a frame_num gap decodes on, repeating its picture until the
    /// frame predicting past the run. Not numbered, so it is neither a frame this peer holds
    /// nor one a later frame may predict from. `None` where the frame cannot stand in.
    fn stand_in(&mut self, data: &Bytes) -> Option<Vec<Payload>> {
        if !self.numbered() {
            return None;
        }
        let skips = self.skips.as_mut()?;
        let nals = h264::split_bitstream(data);
        let built = skips.stand_in(
            nals.iter()
                .map(|n| &n[..n.len().min(h264_skip::HEAD_BYTES)]),
        )?;
        self.stand_ins += 1;
        let nals: Vec<Bytes> = built.into_iter().map(Bytes::from).collect();
        Some(h264::packetize(&nals))
    }

    /// Pack one frame into RTP and send it through the transport, or leave it out. A frame
    /// this peer is not sent leaves its picture behind the screen, so it clears the frame a
    /// key-frame request would read as on the wire (`still_request`).
    pub fn push_frame(
        &mut self,
        frame: EncodedFrame,
        transport: &mut dyn SenderTransport,
        now: f64,
    ) {
        if !self.started {
            return;
        }
        if !self.send_frame(frame, transport, now) {
            self.frame_left = None;
        }
    }

    /// `push_frame`; false for a frame that did not go out.
    fn send_frame(
        &mut self,
        frame: EncodedFrame,
        transport: &mut dyn SenderTransport,
        now: f64,
    ) -> bool {
        if !self.enabled || self.send_codec.is_none() || self.peer_gone(transport, now) {
            return false;
        }
        let (Some(codec_kind), Some((payload_type, clock_rate))) = (
            self.send_codec_kind,
            self.send_codec
                .as_ref()
                .map(|c| (c.payload_type, c.clock_rate)),
        ) else {
            return false;
        };
        // A frame the capture coded before a codec switch is not this codec's to pack.
        if frame.codec.is_some_and(|c| c != codec_kind) {
            return false;
        }
        if self.packetizer.is_none() {
            let codec = self.send_codec.as_ref().expect("checked above");
            self.packetizer = Packetizer::for_codec(codec, self.red_distance);
        }
        let Some(packetizer) = self.packetizer.as_mut() else {
            return false;
        };
        let packed = match packetizer.pack(&frame.data, frame.timestamp) {
            Ok(packed) => packed,
            Err(e) => {
                // A frame its packer cannot read is dropped, not the sender with it.
                log::warn!(
                    target: "webrtc",
                    "{} sender {}: dropped a frame it cannot pack: {e}",
                    self.kind.as_str(),
                    self.ssrc
                );
                if self.kind == Kind::Video {
                    self.hooks.pli();
                }
                return false;
            }
        };
        if packed.payloads.is_empty() {
            return false;
        }
        let keyframe = frame.keyframe;
        let now_ns = (now * 1e9) as i64;
        let instant_ns = match frame.timing {
            Some(t) if t.capture_ns > 0 => t.capture_ns,
            _ => now_ns,
        };
        if self.kind == Kind::Video && keyframe {
            // Feeds the pacer's IDR-aware queue budget and resurrects video after a GOP reset.
            // The pre-encoded path cannot tell a forced key frame from a natural one; the
            // pacer reads that from when it last asked (`Pacer::note_keyframe`). Every JPEG
            // frame is a key frame, so every one feeds the floor.
            let size: usize = packed.payloads.iter().map(|p| p.len()).sum();
            self.keyframe_bytes = Some(size);
            if let Some(pacer) = transport.pacer() {
                pacer.note_keyframe(size, true, now);
            }
        }
        let timestamp = self.timestamp_origin.wrapping_add(frame.timestamp);
        let mut payloads = packed.payloads;
        let mut described: Option<(u16, Option<u16>)> = None;
        let mut standing_in = false;
        let descriptor = self.map.has_dependency_descriptor();
        if let (Some((frame_id, reference)), true) = (frame.dependency, self.numbers_frames()) {
            if self.forward(frame_id, reference, keyframe, frame.anchor, transport, now) {
                described = self.describe(frame_id, reference, keyframe);
                if described.is_none() {
                    self.undecodable(frame_id, reference);
                    return false;
                }
                if !descriptor && keyframe && self.numbered() {
                    self.learn_parameter_sets(&frame.data);
                }
            } else {
                self.hooks.on_frame_left_out();
                let stand_in = if descriptor {
                    None
                } else {
                    self.stand_in(&frame.data)
                };
                let Some(stand_in) = stand_in else {
                    return false;
                };
                // The frame that is not sent leaves the screen behind whatever stands in.
                self.frame_left = None;
                payloads = stand_in;
                standing_in = true;
            }
        }
        // abs-capture-time rides the first packet of a key frame and of a frame a second
        // after the last one, as libwebrtc paces it: the RTP clock follows the capture, so a
        // receiver extrapolates between them.
        let mut abs_capture_time = None;
        if self.kind == Kind::Video
            && (keyframe || instant_ns - self.last_abs_capture_ns >= 1_000_000_000)
        {
            self.last_abs_capture_ns = instant_ns;
            abs_capture_time = Some(ntp_from_monotonic_ns(instant_ns));
        }
        let abs_send_time = ((ntp_from_monotonic_ns(now_ns) >> 14) & 0x00FF_FFFF) as u32;
        // The color signal rides every packet of a key frame, as libwebrtc sends it; the
        // receiver keeps it for the frames that follow.
        let color_space = if keyframe {
            rtp_color_space(codec_kind)
        } else {
            None
        };
        let history_frame = match (described, descriptor) {
            (Some(_), true) => frame.dependency.map(|(fid, _)| fid),
            _ => None,
        };
        // The actual response's packets start a fresh repair epoch. Their predecessor's packets
        // must not consume the queue behind this response, but the response itself stays repairable.
        if self.kind == Kind::Video
            && keyframe
            && codec_kind == Codec::H264
            && std::mem::take(&mut self.response_history_pending)
        {
            self.history.abandon();
        }
        // Every datagram of the frame is built before the first is sent: building and sending
        // in turn evicts what the other just warmed, and the frame is decodable only once its
        // last packet is in.
        let count = payloads.len();
        let mut outgoing: Vec<Built> = Vec::with_capacity(count + count / FEC_GROUP_PACKETS + 3);
        let mut protected: Vec<(u16, usize)> = Vec::new();
        for (i, payload) in payloads.into_iter().enumerate() {
            let last = i + 1 == count;
            let size = payload.len();
            let mut packet =
                RtpPacket::new(payload_type, self.sequence_number, timestamp, self.ssrc);
            packet.marker = last;
            let extensions = &mut packet.extensions;
            extensions.abs_send_time = Some(abs_send_time);
            extensions.mid = self.mid.clone();
            let twcc_seq = transport.twcc().next(size, now);
            extensions.transport_sequence_number = Some(twcc_seq);
            // Min and max 0 hint the receiver to render frames as soon as possible.
            extensions.playout_delay = Some((0, 0));
            extensions.color_space = color_space;
            // Video-timing rides the last packet of a frame, about five times a second like
            // libwebrtc's timer-triggered frames.
            if last && self.kind == Kind::Video && now - self.last_video_timing >= 0.2 {
                self.last_video_timing = now;
                extensions.video_timing = Some(video_timing_legs(frame.timing, now_ns, 0));
            }
            if let (Some((number, fdiff)), true) = (described, descriptor) {
                extensions.dependency_descriptor =
                    Some(dependency_descriptor(i == 0, last, number, fdiff, keyframe));
            }
            if i == 0 {
                extensions.abs_capture_time = abs_capture_time;
            }
            packet.payload = payload;
            let media_seq = packet.sequence_number;
            self.sequence_number = self.sequence_number.wrapping_add(1);
            let mut header = Vec::with_capacity(64);
            packet.write_header(&self.map, &mut header);
            let mut repairs = Vec::new();
            if let Some(fec_pt) = self.fec_payload_type {
                if self.fec_group.is_empty() {
                    self.fec_first_seq = media_seq;
                }
                let mut normalized = header.clone();
                self.map.zero_mutable(&mut normalized);
                self.fec_group.push((normalized, packet.payload.clone()));
                if last || self.fec_group.len() == FEC_GROUP_PACKETS {
                    repairs = self.close_fec_group(fec_pt, timestamp, &mut protected, now);
                }
            }
            outgoing.push(Built {
                header,
                payload: packet.payload.clone(),
                twcc_seq: Some(twcc_seq),
                media_seq: Some(media_seq),
                size,
            });
            self.keep_padding_source(&packet);
            self.history.add(packet, now, history_frame);
            outgoing.extend(repairs.into_iter().map(|repair| Built {
                header: repair,
                payload: Payload::default(),
                twcc_seq: None,
                media_seq: None,
                size: 0,
            }));
        }
        let last_media = outgoing
            .iter()
            .rev()
            .find(|b| b.media_seq.is_some())
            .and_then(|b| b.twcc_seq);
        // A stand-in is no frame of the stream's: the pacer counts no frame for it, and the
        // peer's frame was told left out already.
        if let (Some(last), Kind::Video, false) = (last_media, self.kind, standing_in) {
            self.frame_handed = Some(last);
            let size: usize = outgoing.iter().map(|b| b.size).sum();
            let frame_id = described.and(frame.dependency.map(|(fid, _)| fid));
            self.note_frame_end(
                last,
                FrameEnd {
                    keyframe,
                    timing: frame.timing,
                    captured_ns: frame.timing.map_or(0, |t| t.capture_ns),
                    size,
                    packets: outgoing.len() as u32,
                    frame_id,
                },
            );
            if let Some(pacer) = transport.pacer() {
                pacer.frame_end_with_keyframe(last, keyframe);
            }
            if let (Some(_), Some((frame_id, reference))) = (described, frame.dependency) {
                self.in_flight.push_back((last, frame_id));
                while self.in_flight.len() > IN_FLIGHT_MAX {
                    self.in_flight.pop_front();
                }
                if keyframe {
                    self.key_sent_at = Some(now);
                    self.unheld.clear();
                    self.held.clear();
                    self.unheld_gap = false;
                    self.hooks.on_key_sent();
                }
                self.unheld.push_back(Unheld {
                    frame_id,
                    reference,
                    keyframe,
                    sent_at: now,
                    packets: outgoing
                        .iter()
                        .filter_map(|b| Some((b.twcc_seq?, b.media_seq?)))
                        .collect(),
                });
                while self.unheld.len() > UNHELD_MAX {
                    self.unheld.pop_front();
                }
            }
            self.hooks.on_frame_queued();
        }
        let resyncs = self.resyncs;
        let mut packets = outgoing.into_iter();
        while let Some(built) = packets.next() {
            if self.resyncs != resyncs {
                // A pacer reset cut this frame: the rest of it would only queue in front of
                // the frame that resyncs the peer, and goes as the packets the pacer drops do.
                let twcc = transport.twcc();
                for seq in std::iter::once(built)
                    .chain(packets.by_ref())
                    .filter_map(|b| b.twcc_seq)
                {
                    twcc.dropped(seq);
                }
                if let Some(last) = last_media {
                    self.forget_frame_end(last);
                    if let Some(pacer) = transport.pacer() {
                        pacer.forget_frame_end(last);
                    }
                }
                break;
            }
            self.send(
                &built.header,
                &built.payload,
                built.twcc_seq,
                transport,
                now,
            );
            if built.media_seq.is_some() {
                self.octet_count += built.size as u64;
                self.packet_count += 1;
            }
        }
        if !protected.is_empty() {
            let repaired = now + transport.send_delay();
            for (first, count) in protected {
                for offset in 0..count {
                    self.history
                        .repaired(first.wrapping_add(offset as u16), repaired);
                }
            }
        }
        self.rtp_timestamp = timestamp;
        self.rtp_instant_ns = instant_ns;
        self.rtp_clock_rate = clock_rate;
        true
    }

    /// Close the open FlexFEC group: the repairs `fec_repairs` grants it, interleaved, and the
    /// group noted in `protected` where it took any.
    fn close_fec_group(
        &mut self,
        fec_pt: u8,
        timestamp: u32,
        protected: &mut Vec<(u16, usize)>,
        now: f64,
    ) -> Vec<Vec<u8>> {
        let count = self.fec_group.len();
        let mut repairs = self.fec_repairs(count);
        let largest = self
            .fec_group
            .iter()
            .map(|(h, p)| h.len() + p.len())
            .max()
            .unwrap_or(0);
        let mut out = Vec::with_capacity(repairs);
        for repair in 0..repairs {
            if !self.repair_room(largest, true, now) {
                repairs = repair;
                break;
            }
            let header = RepairHeader {
                first_sequence_number: self.fec_first_seq,
                protected_ssrc: self.ssrc,
                payload_type: fec_pt,
                sequence_number: self.fec_sequence_number,
                timestamp,
                ssrc: self.fec_ssrc,
            };
            out.push(build_flexfec_03(
                &self.fec_group,
                &header,
                &interleaved(repair, count, repairs),
            ));
            self.fec_sequence_number = self.fec_sequence_number.wrapping_add(1);
        }
        if repairs > 0 {
            protected.push((self.fec_first_seq, count));
        }
        self.fec_group.clear();
        out
    }

    /// Hold what a frame's end reports, as far back as the pacer holds frame ends.
    fn note_frame_end(&mut self, tag: u16, end: FrameEnd) {
        self.frame_ends.push_back((tag, end));
        while self.frame_ends.len() > FRAME_ENDS_MAX {
            self.frame_ends.pop_front();
        }
    }

    fn forget_frame_end(&mut self, tag: u16) -> Option<FrameEnd> {
        let pos = self.frame_ends.iter().position(|(t, _)| *t == tag)?;
        self.frame_ends.remove(pos).map(|(_, end)| end)
    }

    /// Note that a frame this peer is not sent leaves no frame on the wire to wait on
    /// (`still_request`): the engine calls it for a frame it withholds before `push_frame`.
    pub fn frame_withheld(&mut self) {
        self.frame_left = None;
    }

    /// Answer one RTCP packet routed to this sender.
    pub fn handle_rtcp(
        &mut self,
        packet: &RtcpPacket,
        transport: &mut dyn SenderTransport,
        now: f64,
    ) {
        match packet {
            RtcpPacket::Rr { reports, .. } | RtcpPacket::Sr { reports, .. } => {
                for report in reports.iter().filter(|r| r.ssrc == self.ssrc) {
                    if self.lsr == Some(report.lsr) && report.dlsr != 0 {
                        let rtt = now - self.lsr_time - report.dlsr as f64 / 65536.0;
                        self.rtt = Some(match self.rtt {
                            None => rtt,
                            Some(old) => RTT_ALPHA * old + (1.0 - RTT_ALPHA) * rtt,
                        });
                    }
                }
            }
            RtcpPacket::Rtpfb(fb) if fb.fmt == RTCP_RTPFB_NACK => {
                let mut gone = self.past_repair(&fb.lost, transport);
                if gone {
                    self.history.abandon();
                }
                for &seq in &fb.lost {
                    if self.history.abandoned(seq) {
                        continue;
                    }
                    let Some((sent, frame, times)) = self.history.nacked(seq) else {
                        // A list runs oldest first, so one let go says nothing about the rest.
                        gone = true;
                        continue;
                    };
                    let sent = sent.clone();
                    if !self.repair_room(
                        sent.payload.len() * if times == 1 { 2 } else { 1 },
                        false,
                        now,
                    ) {
                        break;
                    }
                    // A NACK after the first, sent once the retransmission and any FlexFEC
                    // repair of the packet had a round trip to reach the peer, says neither
                    // did: the frame is lost to it.
                    let lost = times > 1
                        && frame.is_some()
                        && self.history.unrepaired(seq, now, self.rtt.unwrap_or(0.0));
                    if !self.retransmit(sent.clone(), transport, now) {
                        break;
                    }
                    // A first repair goes out twice, back to back: lost with the original, it
                    // would cost the frame a second NACK a round trip later.
                    if times == 1 && !self.retransmit(sent, transport, now) {
                        break;
                    }
                    let delay = transport.send_delay();
                    self.history.repaired(seq, now + delay);
                    if let (true, Some(frame)) = (lost, frame)
                        && self.history.newly_lost(frame, now)
                    {
                        self.hooks.lost_frame(frame);
                        self.lost_at = Some(now);
                    }
                }
                if gone && !self.resync_held(transport, now) && !self.key_on_its_way(transport, now)
                {
                    // Gone from the history, or past repair, where no resync answers it: only
                    // a key frame brings the peer back, unless one is on its way.
                    self.ask_key_frame(transport, now);
                }
            }
            RtcpPacket::Xr(xr) if xr.rrtr.is_some() => {
                let rrtr = xr.rrtr.expect("checked");
                self.rrtrs.remove(&xr.ssrc);
                while self.rrtrs.len() >= RRTR_REPORTERS_MAX {
                    self.rrtrs.pop_oldest();
                }
                self.rrtrs.insert(
                    xr.ssrc,
                    (((rrtr >> 16) & 0xFFFF_FFFF) as u32, (now * 1e9) as i64),
                );
            }
            RtcpPacket::Rtpfb(fb) if fb.fmt == RTCP_RTPFB_TWCC => {
                let probe = transport
                    .pacer()
                    .map(|p| p.burst_probe_bytes())
                    .unwrap_or(0.0);
                let steer = transport.twcc().process_feedback(&fb.fci, now, probe);
                if let (Some(steer), Some(pacer)) = (steer, transport.pacer()) {
                    pacer.set_goodput_bps(steer.goodput_bps, now);
                    pacer.set_link_bps(steer.link_bps, now);
                }
                if !self.unheld.is_empty() {
                    self.confirm(transport, now);
                }
            }
            RtcpPacket::Psfb(fb) if fb.fmt == RTCP_PSFB_PLI || fb.fmt == RTCP_PSFB_FIR => {
                // A Full Intra Request (RFC 5104) asks what a PLI does: a key frame.
                if !self.still_request(transport, now)
                    && !self.resync_held(transport, now)
                    && !self.key_on_its_way(transport, now)
                {
                    self.ask_key_frame(transport, now);
                }
            }
            RtcpPacket::Psfb(fb) if fb.fmt == RTCP_PSFB_APP => {
                if let Ok((bitrate, ssrcs)) = unpack_remb_fci(&fb.fci)
                    && ssrcs.contains(&self.ssrc)
                {
                    log::debug!(target: "webrtc", "{} sender {}: receiver estimated maximum bitrate {bitrate} bps", self.kind.as_str(), self.ssrc);
                }
            }
            _ => {}
        }
    }

    /// The NTP timestamp of `now_ns` and the RTP timestamp the media clock reads then,
    /// carried forward from the last frame sent (RFC 3550 6.4.1): a report pairs the instant
    /// it is sent with the RTP clock at that instant, not with the last packet's timestamp,
    /// which stands for its capture. Zero before any frame, which a receiver takes for no
    /// mapping.
    fn sender_clock(&self, now_ns: i64) -> (u64, u32) {
        if self.rtp_instant_ns == 0 {
            return (0, 0);
        }
        let elapsed =
            (now_ns - self.rtp_instant_ns) as i128 * self.rtp_clock_rate as i128 / 1_000_000_000;
        (
            ntp_from_monotonic_ns(now_ns),
            (self.rtp_timestamp as i128 + elapsed) as u32,
        )
    }

    /// The DLRR answering each reporter's last reference time, sent beside every sender
    /// report rather than once per reference time: libwebrtc drops the round trip it reports
    /// for a stream at a sender report arriving without one.
    pub fn dlrr_report(&self, now_ns: i64) -> XrPacket {
        XrPacket {
            ssrc: self.ssrc,
            rrtr: None,
            dlrr: self
                .rrtrs
                .iter()
                .map(|(ssrc, (lrr, arrived_ns))| {
                    (
                        ssrc,
                        *lrr,
                        ((((now_ns - arrived_ns) as i128) << 16) / 1_000_000_000) as u32,
                    )
                })
                .collect(),
        }
    }

    /// The compound RTCP packet due every 0.5 to 1.5 s: the sender report, the SDES with the
    /// cname, and the DLRR after it as libwebrtc sends it. The driver sends it through the
    /// transport.
    pub fn rtcp_report(&mut self, now: f64) -> Vec<u8> {
        let now_ns = (now * 1e9) as i64;
        let (ntp_timestamp, rtp_timestamp) = self.sender_clock(now_ns);
        let mut compound = RtcpPacket::Sr {
            ssrc: self.ssrc,
            sender_info: SenderInfo {
                ntp_timestamp,
                rtp_timestamp,
                packet_count: self.packet_count as u32,
                octet_count: self.octet_count as u32,
            },
            reports: vec![],
        }
        .serialize();
        self.lsr = Some(((ntp_timestamp >> 16) & 0xFFFF_FFFF) as u32);
        self.lsr_time = now;
        if let Some(cname) = &self.cname {
            compound.extend(
                RtcpPacket::Sdes {
                    chunks: vec![SourceInfo {
                        ssrc: self.ssrc,
                        items: vec![(1, cname.as_bytes().to_vec())],
                    }],
                }
                .serialize(),
            );
        }
        if !self.rrtrs.is_empty() {
            compound.extend(RtcpPacket::Xr(self.dlrr_report(now_ns)).serialize());
        }
        compound
    }

    /// Send the report due (`rtcp_report`), and how long until the next, randomized over 0.5
    /// to 1.5 s as RFC 3550 6.3 asks.
    pub fn send_report(&mut self, transport: &mut dyn SenderTransport, now: f64) -> f64 {
        let report = self.rtcp_report(now);
        transport.send_rtcp(&report, now);
        0.5 + rand::rng().random::<f64>()
    }

    /// Stop the sender and say so with a BYE.
    pub fn stop(&mut self, transport: &mut dyn SenderTransport, now: f64) {
        self.started = false;
        let bye = RtcpPacket::Bye {
            sources: vec![self.ssrc],
        }
        .serialize();
        transport.send_rtcp(&bye, now);
    }

    /// Whether the sender was started.
    pub fn started(&self) -> bool {
        self.started
    }

    /// The packets kept for retransmission.
    pub fn history(&self) -> &RtpHistory {
        &self.history
    }
}

#[cfg(test)]
#[path = "sender_tests.rs"]
mod tests;
