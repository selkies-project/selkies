# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright (c) Jeremy Lainé.
#   All rights reserved.
#
#   Redistribution and use in source and binary forms, with or without
#   modification, are permitted provided that the following conditions are met:
#
#       * Redistributions of source code must retain the above copyright notice,
#       this list of conditions and the following disclaimer.
#       * Redistributions in binary form must reproduce the above copyright notice,
#       this list of conditions and the following disclaimer in the documentation
#       and/or other materials provided with the distribution.
#       * Neither the name of aiortc nor the names of its contributors may
#       be used to endorse or promote products derived from this software without
#       specific prior written permission.
#
#   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
#   ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
#   WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
#   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
#   FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
#   DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
#   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
#   CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
#   OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
#   OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

import asyncio
import logging
import random
import time
import traceback
import uuid
from collections import deque
from collections.abc import Callable
from typing import Any, Optional, Union


from . import clock, rtp
from .pacer import CLASS_AUDIO, CLASS_VIDEO
from .codecs import get_capabilities, get_encoder, is_rtx
from .codecs.base import Encoder
from .codecs.h264 import H264Encoder
from .codecs.h264_skip import HEAD_BYTES, SkipPictures
from .exceptions import InvalidStateError
from .mediastreams import MediaStreamError, MediaStreamTrack
from .rtcdtlstransport import RTCDtlsTransport
from .rtcrtpparameters import (
    RTCRtpCapabilities,
    RTCRtpCodecParameters,
    RTCRtpSendParameters,
)
from .rtp import (
    RTCP_PSFB_APP,
    RTCP_PSFB_FIR,
    RTCP_PSFB_PLI,
    RTCP_RTPFB_NACK,
    RTCP_RTPFB_TWCC,
    RtpHistory,
    AnyRtcpPacket,
    RtcpByePacket,
    RtcpPsfbPacket,
    RtcpRrPacket,
    RtcpRtpfbPacket,
    RtcpSdesPacket,
    RtcpSenderInfo,
    RtcpSourceInfo,
    RtcpSrPacket,
    RtcpXrPacket,
    RtpPacket,
    dependency_descriptor,
    unpack_remb_fci,
    wrap_rtx,
    build_flexfec_03,
)
from .stats import (
    RTCOutboundRtpStreamStats,
    RTCRemoteInboundRtpStreamStats,
    RTCStatsReport,
)
from .utils import random16, random32, uint16_add, uint32_add
from pyee.asyncio import AsyncIOEventEmitter

logger = logging.getLogger(__name__)

RTT_ALPHA = 0.85
# Receive-only reporters answered per sender report (libwebrtc answers at most 50 per XR).
RRTR_REPORTERS_MAX = 50
# How long a peer may send nothing at all before this sender stops: a browser sends
# feedback for whatever it receives and checks consent on its path every few seconds
# (2.5 s in libwebrtc, about 5 s in Firefox's stack) even while a still screen sends it
# nothing, so a peer silent this long is gone (asleep, off the network), where ICE
# consent would keep the stream going to it for half a minute.
PEER_SILENCE_S = 10.0
# A libwebrtc receiver with no frame to decode for three times the offer's rtx-time
# (375 ms), and a packet in the last five seconds, asks for a key frame, and again
# each 375 ms: a still screen sends nothing, so each still was answered with a key
# frame and the cleanup after it, which ends in another still. A request this long
# past the round trip after an acknowledged frame, with nothing lost since, is that
# wait; one this soon after the last is a failed decode, which asks each rtx-time.
STILL_REQUEST_S = 0.25

# Where the encoder names each frame's reference and the peer reads the dependency
# descriptor, a peer that does not own its display is served as a selective forwarding
# unit serves a receiver on a narrow link (`_forward`): a delta frame finding
# RESYNC_ROOM_FRAMES of the peer's frames in its pacer, or RESYNC_LAG_S of wait there, is
# left out with the frames predicting from it, before any is numbered. The encoder is told
# the run's first frame is lost once the peer has room, or sooner where waiting would cost
# a key frame: at RESYNC_REACH_FRAMES, while it still holds the frame to predict from (eight
# back), and in H.264 RESYNC_WRAP_LEAD frames ahead of a frame_num wrap (FRAME_NUM_WRAP
# frames past a key frame, a frame never left out), unless the peer is far behind:
# RESYNC_FAR_FRAMES or RESYNC_FAR_S, or a path that lost packets within RESYNC_LOSS_S while a
# queue stood on it, which says it drops what overflows its buffer rather than queueing it.
# A run the pacer cut (`_pacer_resync`) for such a peer is answered the same way, and one the
# encoder has not predicted past in RESYNC_S costs a key frame, as the WebSockets page's decode
# gate waits (LOST_RECOVERY_MS in lib/decode-gate.js).
# Where the encoder is told which frames every peer was sent (`on_frame_out`, its last packet
# on the wire) and holds (`on_frame_held`: transport-cc reported each of its packets received,
# or the retransmission of one, within HELD_S, and the frame it predicts from is held), it pins
# the newest anchor every peer holds, so a run of any depth is predicted past on its report,
# and flags anchors predicting from a frame every peer was sent (`RTCEncodedFrame.anchor`):
# such a frame goes to a peer without room as a key frame does, unless the peer is far behind,
# and ends its run. A frame is numbered for FRAME_NUMBERS_MEMORY frames, as far back as a
# pinned anchor may be. A peer that lost frames past repair (a NACK for packets gone from the
# history, or its own PLI) is resynced from the newest frame it holds as a cut run is, where
# anchors run (`_resync_held`); a stall of such requests past RESYNC_ANCHORED_S, from a decoder
# that only takes a key frame, is sent one.
RESYNC_ROOM_FRAMES = 3
RESYNC_LAG_S = 0.1
RESYNC_REACH_FRAMES = 5
RESYNC_WRAP_LEAD = 2
RESYNC_FAR_FRAMES = 12
RESYNC_FAR_S = 1.0
RESYNC_LOSS_S = 0.5
RESYNC_S = 1.0
# Where the encoder pins an anchor every peer holds (an anchor came since the codec's first
# frame, its key frame being the first anchor), it predicts past a run of any depth on the
# report, so a run waits RESYNC_ANCHORED_S for room.
RESYNC_ANCHORED_S = 4.0
# An anchor goes to a peer without room only while the queue standing in front of it is under
# RESYNC_ANCHOR_S: on a link too narrow for the anchors alone they would never let it drain.
RESYNC_ANCHOR_S = 0.25
HELD_S = 8.0
FRAME_NUMBERS_MEMORY = 4096
# The shortest H.264 frame_num range, which every longer one is a multiple of.
FRAME_NUM_WRAP = 16

# Media packets per FlexFEC group at most; a group also closes with its frame.
FEC_GROUP_PACKETS = 10
# Smoothed loss past which every FlexFEC group takes its repairs, read on windows no queue
# stood in for FEC_CLEAR_WINDOWS before them, so a queue's own overflow never counts.
FEC_FULL_LOSS = 0.002
FEC_CLEAR_WINDOWS = 3


def random_sequence_number() -> int:
    """
    Generate a random RTP sequence number.

    The sequence number is chosen in the lower half of the allowed range in
    order to avoid wraparounds which break SRTP decryption.

    See:
    https://chromiumdash.appspot.com/commit/13b327b05fa3788b4daa9c3463e13282824cb320
    """
    return random16() % 32768


#: The colour signal a stream was converted with, as the ITU-T H.273 codes the RTP
#: colour-space header extension carries, for the two codecs whose bitstream cannot state it:
#: BT.709 primaries, transfer, and matrix at limited range, with the BT.601 matrix for VP8,
#: which is held to the only one a keyframe header's single colour-space bit can name. A
#: receiver that reads the extension takes it over the bitstream, so H.264, H.265, and AV1 are
#: left to their own headers rather than told here — theirs carry the range as well, which a
#: 4:4:4 session signals as full and this table has no way to know.
RTP_COLOR_SPACE = {
    "video/vp8": (1, 1, 6, 1),
    "video/vp9": (1, 1, 1, 1),
}


class RTCEncodedFrame:
    def __init__(self, payloads: list[bytes], timestamp: int, audio_level: int,
                 keyframe: bool = False, timing: Optional[tuple] = None,
                 dependency: Optional[tuple] = None, anchor: bool = False, data: Any = None):
        self.payloads = payloads
        self.timestamp = timestamp
        self.audio_level = audio_level
        self.keyframe = keyframe
        self.timing = timing
        self.dependency = dependency
        self.anchor = anchor
        self.data = data


def video_timing_legs(timing: Optional[tuple], now_ns: int, arrival_delta_ms: int) -> tuple:
    """The video-timing extension of a frame packetized at `now_ns`
    (CLOCK_MONOTONIC): the flags byte, timer-triggered, then six millisecond
    legs from the capture. With `timing` as the capture library stamped it
    (capture, encode start, encode end, in the same clock) the encode legs
    are real and the packetization leg is the frame's whole age; without it,
    the encode legs are unknown (0) and the packetization leg is the frame's
    time in the sender, `arrival_delta_ms`. Pacer exit repeats packetization,
    since the stamp is taken ahead of the pacer, and the two network legs
    stay 0, as a sender that is not a middlebox leaves them.
    """
    if not timing or timing[0] <= 0:
        return (0x01, 0, 0, arrival_delta_ms, arrival_delta_ms, 0, 0)
    capture_ns, encode_start_ns, encode_end_ns = timing
    leg = lambda instant_ns: min(0xFFFF, max(0, (instant_ns - capture_ns) // 1_000_000))
    packetized = leg(now_ns)
    return (0x01, leg(encode_start_ns), leg(encode_end_ns), packetized, packetized, 0, 0)


class RTCRtpSender(AsyncIOEventEmitter):
    """
    The :class:`RTCRtpSender` interface provides the ability to control and
    obtain details about how a particular :class:`MediaStreamTrack` is encoded
    and sent to a remote peer.

    :param trackOrKind: Either a :class:`MediaStreamTrack` instance or a
                         media kind (`'audio'` or `'video'`).
    :param transport: An :class:`RTCDtlsTransport`.
    """

    def __init__(
        self, trackOrKind: Union[MediaStreamTrack, str], transport: RTCDtlsTransport
    ) -> None:
        super().__init__()
        if transport.state == "closed":
            raise InvalidStateError

        if isinstance(trackOrKind, MediaStreamTrack):
            self.__kind = trackOrKind.kind
            self.replaceTrack(trackOrKind)
        else:
            self.__kind = trackOrKind
            self.replaceTrack(None)
        self.__cname: Optional[str] = None
        self._ssrc = random32()
        self._rtx_ssrc = random32()
        self._fec_ssrc = random32()
        # Fresh UUID per sender: the msid stream identifier when the caller
        # supplies no MediaStream grouping.
        self._stream_id = str(uuid.uuid4())
        self._enabled = True
        self._peer_silent = False
        # Told each frame once its last packet is on the wire, as
        # `(capture_ns, payload bytes)`, where the owner wants it (stream stats).
        self.on_frame_sent: Optional[Callable[[int, int], None]] = None
        # Told each frame handed to the pacer with that wait, so the owner can count
        # the frames the pacer drops instead, which call nothing.
        self.on_frame_queued: Optional[Callable[[], None]] = None
        self.__encoder: Optional[Encoder] = None
        # The negotiated codecs and the one frames go out as; None drops them.
        self.__codecs: list[RTCRtpCodecParameters] = []
        self.__send_codec: Optional[RTCRtpCodecParameters] = None
        # The codec a switch before the start named, taken out of the answer at the start.
        self.__pending_codec: Optional[str] = None
        self.__force_keyframe = False
        self.__force_keyframe_used = False
        # Last observed keyframe size (bytes) and whether it was a natural
        # IDR: lets a late-attaching pacer bootstrap its IDR floor from the
        # session-start keyframe instead of waiting for the next one.
        self._keyframe_bytes: Optional[int] = None
        self._keyframe_natural: bool = True
        # Whether this peer is served selectively, as one not owning its display
        # (`_forward`); told a run's first frame is lost and whether the run is within the
        # encoder's reach, answering whether the encoder took it (None emits "lost_frame");
        # and told each frame left out, which is neither queued nor sent.
        self.selective: Optional[Callable[[], bool]] = None
        self.on_resync: Optional[Callable[[int, bool], bool]] = None
        self.on_frame_left_out: Optional[Callable[[], None]] = None
        # Described frames handed to the pacer whose last packet has not left, as (tag of
        # that packet, capture frame id), oldest first: a GOP reset cuts these
        # (`_pacer_resync`). And the run of frames this peer is not sent: when it began
        # (None for none), its first frame, how many it holds, whether the encoder took the
        # report, the delta frames since the last key frame, and a count of pacer cuts, by
        # which a frame being sent learns a cut took it.
        self.__in_flight: deque[tuple[int, int]] = deque(maxlen=64)
        # The skipped pictures a peer without the descriptor is sent for the H.264 frames it
        # is not (`_stand_in`), and how many.
        self.__skips: Optional[SkipPictures] = None
        self.stand_ins = 0
        self._resync_since: Optional[float] = None
        self._resync_first: Optional[int] = None
        self._resync_run = 0
        self._resync_told = False
        self._since_key = 0
        self._resyncs = 0
        # Told each described frame on the wire to the peer and each it holds, as `(frame id,
        # key frame)`, and each key frame sent it; the described frames sent and not yet held,
        # oldest first, as (frame id, frame predicted from, key frame, when sent,
        # [(transport-wide, media sequence number)]); the frames held, oldest first, as far
        # back as frames are numbered, since one may predict from a pinned anchor held long
        # ago; whether a frame sent since the key frame was never held (`_confirm`); and the
        # transport-wide sequence number each retransmitted packet last went out under.
        self.on_frame_out: Optional[Callable[[int, bool], None]] = None
        self.on_frame_held: Optional[Callable[[int, bool], None]] = None
        self.on_key_sent: Optional[Callable[[], None]] = None
        self._anchored = False
        self.__unheld: deque[tuple] = deque(maxlen=1024)
        self._stall_since: Optional[float] = None
        self._stall_last = 0.0
        self._held_at = 0.0
        self._key_sent_at: Optional[float] = None
        self.__held: dict[int, None] = {}
        self._unheld_gap = False
        self.__rtx_twcc: dict[int, int] = {}
        self.__loop = asyncio.get_running_loop()
        self.__mid: Optional[str] = None
        self.__rtp_exited = asyncio.Event()
        self.__rtp_header_extensions_map = rtp.HeaderExtensionsMap()
        self.__rtp_started = asyncio.Event()
        self.__rtp_task: Optional[asyncio.Future[None]] = None
        self.__rtp_history = RtpHistory()
        # The newest media sequence number sent when the pacer last abandoned a
        # GOP: a NACK for it or anything before names a packet the keyframe replaces.
        self.__last_sequence: Optional[int] = None
        # Frames numbered on the wire for the dependency descriptor, and the number each
        # capture frame id took, which a frame predicting from it is measured against.
        self.__frame_number = 0
        self.__frame_numbers: dict[int, int] = {}
        # The transport-wide sequence number ending the newest frame handed over, and
        # of the newest on the wire with when it left; when the peer last asked for a
        # key frame; and when it lost a frame past repair, until a frame coded after
        # that is on the wire (`_still_request`).
        self._frame_handed: Optional[int] = None
        self._frame_left: Optional[tuple[int, float]] = None
        self._key_asked_at = 0.0
        self._lost_at: Optional[float] = None
        self.__rtcp_exited = asyncio.Event()
        self.__rtcp_started = asyncio.Event()
        self.__rtcp_task: Optional[asyncio.Future[None]] = None
        self.__rtx_payload_type: Optional[int] = None
        self.__rtx_sequence_number = random_sequence_number()
        self.__fec_payload_type: Optional[int] = None
        self.__fec_sequence_number = random_sequence_number()
        # Repair packets per FlexFEC group, with interleaved masks: as many
        # losses in a group as there are repairs are recovered. Follows the
        # loss `steer_fec` is told about.
        self.fec_repair_packets = 1
        self._fec_loss = 0.0
        # The loss of windows clear of a queue, and how many windows in a row
        # have been (`steer_fec`).
        self._fec_clear_loss = 0.0
        self._fec_clear = 0
        # Whether every group takes its repairs (`steer_fec`), and the repairs
        # owed otherwise, in FEC_GROUP_PACKETS-ths (`_fec_repairs`).
        self._fec_full = False
        self._fec_credit = 0
        self.__started = False
        self.__stats = RTCStatsReport()
        self.__transport = transport

        # stats
        self.__lsr: Optional[int] = None
        self.__lsr_time: Optional[float] = None
        # The RTP timestamp of the last frame sent, the CLOCK_MONOTONIC instant it
        # stands for (its capture, where the frame says), and its clock rate: a
        # sender report maps the RTP clock of the moment it is sent from these.
        self.__rtp_timestamp = 0
        self.__rtp_instant_ns = 0
        self.__rtp_clock_rate = 0
        self.__octet_count = 0
        self.__packet_count = 0
        self.__rtt: Optional[float] = None
        # The last reference time each receive-only reporter sent (middle 32 bits of its
        # NTP time) and the CLOCK_MONOTONIC instant it arrived, answered in every report.
        self.__rrtrs: dict[int, tuple[int, int]] = {}

        # logging
        self.__log_debug: Callable[..., None] = lambda *args: None
        if logger.isEnabledFor(logging.DEBUG):
            self.__log_debug = lambda msg, *args: logger.debug(
                f"RTCRtpSender(%s) {msg}", self.__kind, *args
            )

    @property
    def kind(self) -> str:
        return self.__kind

    @property
    def track(self) -> MediaStreamTrack:
        """
        The :class:`MediaStreamTrack` which is being handled by the sender.
        """
        return self.__track

    @property
    def transport(self) -> RTCDtlsTransport:
        """
        The :class:`RTCDtlsTransport` over which media data for the track is
        transmitted.
        """
        return self.__transport

    @classmethod
    def getCapabilities(cls, kind: str) -> RTCRtpCapabilities:
        """
        Returns the most optimistic view of the system's capabilities for
        sending media of the given `kind`.

        :rtype: :class:`RTCRtpCapabilities`
        """
        return get_capabilities(kind)

    async def getStats(self) -> RTCStatsReport:
        """
        Returns statistics about the RTP sender.

        :rtype: :class:`RTCStatsReport`
        """
        self.__stats.add(
            RTCOutboundRtpStreamStats(
                # RTCStats
                timestamp=clock.current_datetime(),
                type="outbound-rtp",
                id="outbound-rtp_" + str(id(self)),
                # RTCStreamStats
                ssrc=self._ssrc,
                kind=self.__kind,
                transportId=self.transport._stats_id,
                # RTCSentRtpStreamStats
                packetsSent=self.__packet_count,
                bytesSent=self.__octet_count,
                # RTCOutboundRtpStreamStats
                trackId=str(id(self.track)),
            )
        )
        self.__stats.update(self.transport._get_stats())

        return self.__stats

    def replaceTrack(self, track: Optional[MediaStreamTrack]) -> None:
        self.__track = track
        if track is not None:
            self._track_id = track.id
        else:
            self._track_id = str(uuid.uuid4())

    def setTransport(self, transport: RTCDtlsTransport) -> None:
        self.__transport = transport

    async def send(self, parameters: RTCRtpSendParameters) -> None:
        """
        Attempt to set the parameters controlling the sending of media.

        :param parameters: The :class:`RTCRtpSendParameters` for the sender.
        """
        if not self.__started:
            self.__cname = parameters.rtcp.cname
            self.__mid = parameters.muxId

            # make note of the RTP header extension IDs
            self.__transport._register_rtp_sender(self, parameters)
            self.__rtp_header_extensions_map.configure(parameters)

            # Send with the first codec that actually has an encoder: auxiliary
            # entries (rtx, flexfec) can top the negotiated list when the media
            # codec was filtered out, and starting RTP on them kills the sender.
            send_codec = None
            for codec in parameters.codecs:
                if is_rtx(codec) or codec.mimeType.lower() == "video/flexfec-03":
                    continue
                send_codec = codec
                break
            if send_codec is None:
                raise InvalidStateError("No sendable media codec was negotiated")
            self.__codecs = list(parameters.codecs)
            if self.__pending_codec is not None:
                send_codec = self.negotiated_codec(self.__pending_codec) or send_codec
            self.__send_codec = send_codec
            self.__rtx_payload_type = self._rtx_payload_type_for(send_codec)

            # make note of the FlexFEC payload type (negotiated => protect video)
            for codec in parameters.codecs:
                if codec.mimeType.lower() == "video/flexfec-03":
                    self.__fec_payload_type = codec.payloadType
                    break

            self.__rtp_task = asyncio.ensure_future(self._run_rtp())
            self.__rtcp_task = asyncio.ensure_future(self._run_rtcp())
            self.__started = True

    def _rtx_payload_type_for(self, codec: RTCRtpCodecParameters) -> Optional[int]:
        for candidate in self.__codecs:
            if is_rtx(candidate) and candidate.parameters["apt"] == codec.payloadType:
                return candidate.payloadType
        return None

    def negotiated_codec(self, mime_type: str) -> Optional[RTCRtpCodecParameters]:
        """The negotiated media codec of `mime_type`, or None when the peer did not take it."""
        wanted = mime_type.lower()
        for codec in self.__codecs:
            if codec.mimeType.lower() == wanted:
                return codec
        return None

    def switch_codec(self, mime_type: str) -> bool:
        """Send the track's frames as the negotiated codec of `mime_type` from
        now on: its payload type, its RTX type, and its own packer. A codec the
        peer never took drops the frames instead, until a switch names one it
        did; before the sender starts, the switch names the codec it starts on,
        out of the answer."""
        if not self.__started:
            self.__pending_codec = mime_type
            return True
        codec = self.negotiated_codec(mime_type)
        self.__send_codec = codec
        self.__encoder = None
        self._anchored = False
        self.__rtx_payload_type = self._rtx_payload_type_for(codec) if codec else None
        return codec is not None

    async def stop(self) -> None:
        """
        Irreversibly stop the sender.
        """
        if self.__started:
            self.__transport._unregister_rtp_sender(self)

            # shutdown RTP and RTCP tasks
            await asyncio.gather(self.__rtp_started.wait(), self.__rtcp_started.wait())
            self.__rtp_task.cancel()
            self.__rtcp_task.cancel()
            await asyncio.gather(self.__rtp_exited.wait(), self.__rtcp_exited.wait())

    async def _handle_rtcp_packet(self, packet: AnyRtcpPacket) -> None:
        if isinstance(packet, (RtcpRrPacket, RtcpSrPacket)):
            for report in filter(lambda x: x.ssrc == self._ssrc, packet.reports):
                # estimate round-trip time
                if self.__lsr == report.lsr and report.dlsr:
                    rtt = time.time() - self.__lsr_time - (report.dlsr / 65536)
                    if self.__rtt is None:
                        self.__rtt = rtt
                    else:
                        self.__rtt = RTT_ALPHA * self.__rtt + (1 - RTT_ALPHA) * rtt

                self.__stats.add(
                    RTCRemoteInboundRtpStreamStats(
                        # RTCStats
                        timestamp=clock.current_datetime(),
                        type="remote-inbound-rtp",
                        id="remote-inbound-rtp_" + str(id(self)),
                        # RTCStreamStats
                        ssrc=packet.ssrc,
                        kind=self.__kind,
                        transportId=self.transport._stats_id,
                        # RTCReceivedRtpStreamStats
                        packetsReceived=self.__packet_count - report.packets_lost,
                        packetsLost=report.packets_lost,
                        jitter=report.jitter,
                        # RTCRemoteInboundRtpStreamStats
                        roundTripTime=self.__rtt,
                        fractionLost=report.fraction_lost,
                    )
                )
        elif isinstance(packet, RtcpRtpfbPacket) and packet.fmt == RTCP_RTPFB_NACK:
            gone = self._past_repair(packet.lost)
            if gone:
                self.__rtp_history.abandon()
            for seq in packet.lost:
                if self.__rtp_history.abandoned(seq):
                    continue
                sent, frame, times = self.__rtp_history.nacked(seq)
                if sent is None:
                    # A list runs oldest first, so one let go says nothing about the rest.
                    gone = True
                    continue
                # A NACK after the first, sent once the retransmission and any FlexFEC
                # repair of the packet had a round trip to reach the peer, says neither
                # did: the frame is lost to it, and the encoder is told once so the frames
                # after it stop predicting from it.
                lost = (times > 1 and frame is not None
                        and self.__rtp_history.unrepaired(seq, time.time(), self.__rtt or 0.0))
                if not await self._retransmit(sent):
                    break
                # A first repair goes out twice, back to back: lost with the original, it would
                # cost the frame a second NACK a round trip later and a prediction past it.
                if times == 1 and not await self._retransmit(sent):
                    break
                self.__rtp_history.repaired(seq, time.time() + self.transport._send_delay())
                if lost and self.__rtp_history.newly_lost(frame, time.time()):
                    self.emit("lost_frame", frame)
                    self._lost_at = time.monotonic()
            if gone and not self._resync_held():
                # Gone from the history, or past repair, where no resync answers it: only a key
                # frame brings the peer back.
                self._emit_pli_event()
        elif isinstance(packet, RtcpXrPacket) and packet.rrtr is not None:
            self.__rrtrs.pop(packet.ssrc, None)
            if len(self.__rrtrs) >= RRTR_REPORTERS_MAX:
                del self.__rrtrs[next(iter(self.__rrtrs))]
            self.__rrtrs[packet.ssrc] = ((packet.rrtr >> 16) & 0xFFFFFFFF, time.monotonic_ns())
        elif isinstance(packet, RtcpRtpfbPacket) and packet.fmt == RTCP_RTPFB_TWCC:
            self.transport._twcc_process_feedback(packet.fci)
            if self.__unheld:
                self._confirm()
        elif isinstance(packet, RtcpPsfbPacket) and packet.fmt in (RTCP_PSFB_PLI, RTCP_PSFB_FIR):
            # A Full Intra Request (RFC 5104) asks what a PLI does: a key frame.
            if not self._still_request() and not self._resync_held():
                self._send_keyframe()
                self._emit_pli_event()
        elif isinstance(packet, RtcpPsfbPacket) and packet.fmt == RTCP_PSFB_APP:
            try:
                bitrate, ssrcs = unpack_remb_fci(packet.fci)
                if self._ssrc in ssrcs:
                    self.__log_debug(
                        "- receiver estimated maximum bitrate %d bps", bitrate
                    )
                    if self.__encoder and hasattr(self.__encoder, "target_bitrate"):
                        self.__encoder.target_bitrate = bitrate
            except ValueError:
                pass

    def _still_request(self) -> bool:
        """Whether a key-frame request is a receiver's wait on a still screen
        (STILL_REQUEST_S), which a key frame would only restart: the newest frame is
        on the wire and acknowledged, it left STILL_REQUEST_S and the round trip ago,
        no frame lost past repair awaits one predicting past it, and the request
        before came STILL_REQUEST_S or more earlier."""
        now = time.monotonic()
        before, self._key_asked_at = self._key_asked_at, now
        left = self._frame_left
        return (left is not None and left[0] == self._frame_handed and self._lost_at is None
                and now - left[1] >= STILL_REQUEST_S + (self.__rtt or 0.0)
                and now - before >= STILL_REQUEST_S
                and self.transport._twcc_acked(left[0]))

    def _frame_on_wire(self, seq: int, keyframe: bool, timing: Optional[tuple],
                       captured: int, size: int, frame_id: Optional[int] = None) -> None:
        """A frame's last packet left for the wire. One whose encode began after the
        peer last lost a frame predicts past it, as a key frame predicts from nothing."""
        if frame_id is not None and self.on_frame_out is not None:
            self.on_frame_out(frame_id, keyframe)
        self._frame_left = (seq, time.monotonic())
        while self.__in_flight and ((seq - self.__in_flight[0][0]) & 0xFFFF) < 0x8000:
            self.__in_flight.popleft()
        encoded = timing[1] / 1e9 if timing and len(timing) > 1 and timing[1] > 0 else 0.0
        if self._lost_at is not None and (keyframe or encoded > self._lost_at):
            self._lost_at = None
        if self.on_frame_sent is not None:
            self.on_frame_sent(captured, size)

    def _emit_pli_event(self):
        """
        Emit a "pli" event to notify the application layer, which is
        responsible for instructing the encoder to generate that keyframe
        """
        self.emit("pli")

    def _pacer_resync(self, first_tag: Optional[int]) -> bool:
        """A pacer GOP reset (`RtpPacer._reset_gop`), told the oldest packet it dropped.

        Where the encoder names each frame's reference, the peer reads the
        dependency descriptor and does not own its display (`selective`), the
        frames whose last packet had not left are lost to the peer: they leave
        the frames it was sent, so `_forward` holds back whatever predicts from
        them, NACKs for the dropped packets go unanswered, and the run is
        answered as a run left out is. False where this sender cannot, which
        leaves the pacer its key frame, and for the display's owner and a peer
        without the descriptor: the cut leaves a gap in its sequence numbers
        that no retransmission fills, which its receiver closes on a key frame
        (a burst of loss on a software encoder's stream otherwise took one to
        two seconds to close).
        """
        if (first_tag is None or not self.__in_flight or self.selective is None
                or not self.selective()
                or not self.__rtp_header_extensions_map.has_dependency_descriptor()):
            return False
        lost = [fid for tag, fid in self.__in_flight if ((tag - first_tag) & 0xFFFF) < 0x8000]
        if not lost:
            return False
        for fid in lost:
            self.__frame_numbers.pop(fid, None)
        self.__in_flight.clear()
        self.__rtp_history.abandon()
        self._resyncs += 1
        if self._resync_since is None:
            self._open_run(lost[0])
            self._resync_run = len(lost)
        else:
            self._resync_run += len(lost)
        self._repair()
        return True

    def _confirm(self) -> None:
        """Tell `on_frame_held` of each frame sent the peer now holds: every packet of it
        reported received, or the retransmission of one, and the frame it predicts from
        held. A frame not held within HELD_S never will be. A peer without the dependency
        descriptor decodes nothing past a frame it lacks until a key frame, so it holds a
        frame only once it holds every frame sent before it."""
        acked = self.transport.twcc_arrived
        now = time.monotonic()
        rtx = self.__rtx_twcc
        in_order = not self.__rtp_header_extensions_map.has_dependency_descriptor()
        kept = []
        for entry in self.__unheld:
            frame_id, reference, keyframe, sent_at, packets = entry
            if now - sent_at > HELD_S:
                self._unheld_gap = self._unheld_gap or in_order
                continue
            if (not (in_order and (kept or self._unheld_gap))
                    and all(acked(twcc) or (media in rtx and acked(rtx[media])) for twcc, media in packets)
                    and (keyframe or reference in self.__held)):
                self.__held.pop(frame_id, None)
                self.__held[frame_id] = None
                self._held_at = now
                if len(self.__held) > FRAME_NUMBERS_MEMORY:
                    del self.__held[next(iter(self.__held))]
                if self.on_frame_held is not None:
                    self.on_frame_held(frame_id, keyframe)
            else:
                kept.append(entry)
        self.__unheld.clear()
        self.__unheld.extend(kept)
        if len(rtx) > 1024:
            for media in list(rtx)[:512]:
                del rtx[media]

    def _past_repair(self, lost: list) -> bool:
        """Whether a NACK from a peer that does not own its display, and reads the dependency
        descriptor, names more than its pacer queue holds, a first repair going out twice: the
        burst a link that went dark asks for once it is back, whose retransmissions would
        overflow the queue and cost every peer a key frame, and hold every newer frame behind
        them. It is answered as a loss past repair."""
        if (self.__kind != "video" or self.selective is None or not self.selective()
                or not self.__rtp_header_extensions_map.has_dependency_descriptor()):
            return False
        room = self.transport.video_room()
        held = (self.__rtp_history.get(seq) for seq in lost)
        return room is not None and 2 * sum(len(p.payload) for p in held if p is not None) > room

    def _resync_held(self) -> bool:
        """Answer a peer that lost frames past repair as a run the pacer cut is, where it does
        not own its display and anchors run: the frames sent it after the newest it holds leave
        the frames it was sent, and the encoder is told the first is lost (`_repair`), so a frame
        predicting from one it holds comes next. A request while that run is open, or while the
        key frame it was sent is on its way, is answered by them. False where it cannot, which
        asks for a key frame: past RESYNC_ANCHORED_S of such requests one after another while
        it holds frames it is sent (a decoder that only takes a key frame; one holding nothing
        new between two requests is starved, as through an outage, and starts the count again),
        where it holds nothing and no key frame is due, and for a peer without the dependency
        descriptor, which decodes nothing past the packets it lacks until a key frame. A frame
        held only after a resync let it go is passed over."""
        now = time.monotonic()
        if (self.selective is None or not self.selective() or not self._anchored
                or not self.__rtp_header_extensions_map.has_dependency_descriptor()):
            return False
        if (self._stall_since is None or now - self._stall_last > RESYNC_ANCHORED_S
                or self._held_at < self._stall_last):
            self._stall_since = now
        self._stall_last = now
        if now - self._stall_since > RESYNC_ANCHORED_S:
            self._stall_since = None
            return False
        if self._resync_since is not None:
            return True
        if not self.__held:
            return (self._key_sent_at is not None and now - self._key_sent_at
                    < max(RESYNC_S, 2 * (self._video_backlog()[1] + (self.__rtt or 0.0))))
        number = next((n for n in map(self.__frame_numbers.get, reversed(self.__held)) if n is not None), None)
        if number is None:
            return False
        after = [fid for fid, n in self.__frame_numbers.items() if 0 < ((n - number) & 0xFFFF) < 0x8000]
        if not after:
            return True
        for fid in after:
            self.__frame_numbers.pop(fid, None)
        self._open_run(after[0])
        self._resync_run = len(after)
        self._repair()
        return True

    def _video_backlog(self) -> tuple[int, float, float]:
        """The peer's frames in its pacer, how long a packet waits to reach it, and the
        seconds since its path last lost one (`RTCDtlsTransport.video_backlog`)."""
        backlog = getattr(self.transport, "video_backlog", None)
        return backlog() if backlog is not None else (0, 0.0, float("inf"))

    def _forward(self, frame_id: int, reference: Optional[int], keyframe: bool,
                 anchor: bool = False) -> bool:
        """Whether a frame naming the frame it predicts from goes to this peer; False
        for one left out, or held back for predicting from one (RESYNC_ROOM_FRAMES).
        The first frame predicting past an open run, which decodes on the peer, ends it,
        and an `anchor` goes without room unless the peer is far behind."""
        if keyframe:
            self._since_key = 0
            self._close_run()
            return True
        self._since_key += 1
        self._anchored = self._anchored or anchor
        sent = reference is None or reference in self.__frame_numbers
        # In H.264 a frame where frame_num may wrap is never left out.
        kept = self._numbered() and not self._since_key % FRAME_NUM_WRAP
        if self._resync_since is not None:
            if sent and (not anchor or kept or self._anchor_room()):
                self._close_run()
                return True
            if sent:
                # An anchor left out predicts past the run: the encoder is told of it.
                self._resync_first = frame_id
                self._resync_told = False
            self._resync_run += 1
            self._repair()
            return False
        if not sent or self.selective is None or kept:
            return True
        if anchor and self._anchor_room():
            return True
        frames, wait, _ = self._video_backlog()
        if (frames < RESYNC_ROOM_FRAMES and wait < RESYNC_LAG_S) or not self.selective():
            return True
        self._open_run(frame_id)
        self._repair()
        return False

    def _anchor_room(self) -> bool:
        """Whether this peer is sent an anchor without room: not far behind, and the queue
        in front of it under RESYNC_ANCHOR_S."""
        return not self._far() and self._video_backlog()[1] < RESYNC_ANCHOR_S

    def _far(self) -> bool:
        """Whether this peer is too far behind to be sent a frame without room: its
        pacer holds RESYNC_FAR_FRAMES or RESYNC_FAR_S, or its path lost packets within
        RESYNC_LOSS_S while a queue stood on it."""
        frames, wait, lost = self._video_backlog()
        return (frames >= RESYNC_FAR_FRAMES or wait >= RESYNC_FAR_S
                or (lost < RESYNC_LOSS_S and wait >= RESYNC_LAG_S))

    def _numbered(self) -> bool:
        """Whether the stream is H.264, whose frame_num wraps (FRAME_NUM_WRAP)."""
        codec = self.__send_codec
        return codec is not None and codec.mimeType.lower() == "video/h264"

    def _numbers_frames(self) -> bool:
        """Whether frames naming their reference are numbered for this peer and left out
        where it has no room: one reading the dependency descriptor, or an H.264 one
        without it (Firefox answers so), which finds a frame's reference by sequence
        number, unbroken by a frame left out before it is packed; its frames carry no
        descriptor."""
        return self.__rtp_header_extensions_map.has_dependency_descriptor() or self._numbered()

    def _learn_parameter_sets(self, enc_frame: RTCEncodedFrame) -> None:
        """Keep the SPS and PPS a key frame sent this peer carries, which its stand-ins'
        slice headers are read against (`_stand_in`)."""
        if enc_frame.data is None:
            return
        if self.__skips is None:
            self.__skips = SkipPictures()
        self.__skips.learn([bytes(nal) for nal in H264Encoder._split_bitstream(memoryview(enc_frame.data))])

    def _stand_in(self, enc_frame: RTCEncodedFrame) -> Optional[RTCEncodedFrame]:
        """A picture of skipped macroblocks in place of an H.264 frame left out for a peer
        without the dependency descriptor (`h264_skip`), under the frame's own frame_num and
        reference marking: its receiver hands the decoder every frame it is sent by sequence
        number, and a decoder that refuses a frame_num gap (OpenH264) decodes on, repeating
        its picture until the frame predicting past the run (`_repair`). Not numbered, so it
        is neither a frame this peer holds nor one a later frame may predict from. None where
        the frame cannot stand in, which leaves it out as before."""
        if self.__skips is None or enc_frame.data is None or not self._numbered():
            return None
        built = self.__skips.stand_in(
            [bytes(nal[:HEAD_BYTES]) for nal in H264Encoder._split_bitstream(memoryview(enc_frame.data))])
        if built is None:
            return None
        self.stand_ins += 1
        return RTCEncodedFrame(H264Encoder._packetize(built), enc_frame.timestamp, None, False, enc_frame.timing)

    def _open_run(self, frame_id: int) -> None:
        self._resync_since = time.monotonic()
        self._resync_first = frame_id
        self._resync_run = 1
        self._resync_told = False

    def _close_run(self) -> None:
        self._resync_since = None
        self._resync_first = None
        self._resync_run = 0
        self._resync_told = False

    def _repair(self) -> None:
        """A frame of the open run was held back: tell the encoder of the run's first
        frame once the peer has room, or sooner where waiting would cost a key frame
        (RESYNC_REACH_FRAMES); past RESYNC_S (RESYNC_ANCHORED_S where anchors run), ask
        for a key frame instead."""
        if time.monotonic() - self._resync_since > (RESYNC_ANCHORED_S if self._anchored else RESYNC_S):
            logger.info("RTCRtpSender(%s) frame %s was never predicted past; asking for a key frame",
                        self.__kind, self._resync_first)
            self._close_run()
            self._emit_pli_event()
            return
        if self._resync_told:
            return
        frames, wait, _ = self._video_backlog()
        due = (self._resync_run >= RESYNC_REACH_FRAMES
               or (self._numbered() and -self._since_key % FRAME_NUM_WRAP <= RESYNC_WRAP_LEAD))
        if (frames < RESYNC_ROOM_FRAMES and wait < RESYNC_LAG_S) or (due and not self._far()):
            # Where two anchors run, a run of any depth is predicted past from the one pinned;
            # H.264 keeps one, which its schedule replaces.
            reach = (self._resync_run <= RESYNC_REACH_FRAMES
                     or (self._anchored and not self._numbered()))
            self._lost_at = time.monotonic()
            if self.on_resync is not None:
                self._resync_told = self.on_resync(self._resync_first, reach)
            else:
                self.emit("lost_frame", self._resync_first)
                self._resync_told = True

    def _undecodable(self, frame_id: int, reference: Optional[int]) -> None:
        """A frame predicting from one this peer was never sent (a peer paused across
        it): nothing but a key frame decodes here, and one is asked for."""
        logger.info("RTCRtpSender(%s) frame %s predicts from %s, which this peer "
                    "was not sent; asking for a key frame", self.__kind, frame_id, reference)
        self._emit_pli_event()

    def _peer_gone(self) -> bool:
        """Whether the peer has sent nothing at all for PEER_SILENCE_S, so frames are not
        sent to a client that left without a goodbye. Its first packet after the silence
        asks for a key frame: the frames skipped meanwhile broke the prediction chain, and
        `_describe` already leaves out the ones predicting from them."""
        silent = time.monotonic() - self.transport._peer_heard_at() > PEER_SILENCE_S
        if silent != self._peer_silent:
            self._peer_silent = silent
            logger.info("%s sender %s: the peer %s", self.__kind, self._ssrc,
                        "went silent; sending stopped" if silent else "is back; sending again")
            if not silent:
                self._emit_pli_event()
        return silent

    def _fec_repairs(self, packets: int) -> int:
        """How many repairs a FlexFEC group of `packets` media packets gets:
        `fec_repair_packets`, never more than it has packets, where `steer_fec`
        wants every group repaired. Otherwise `fec_repair_packets` per
        FEC_GROUP_PACKETS media packets, what a group falls short of carried to
        the next: a group closes at every frame, so a low-rate stream's frames
        of one or two packets would each take a repair and double what the
        path carries."""
        if self._fec_full:
            self._fec_credit = 0
            return min(self.fec_repair_packets, packets)
        self._fec_credit += packets * self.fec_repair_packets
        repairs = min(packets, self._fec_credit // FEC_GROUP_PACKETS)
        self._fec_credit -= repairs * FEC_GROUP_PACKETS
        return repairs

    def steer_fec(self, loss_fraction: float, queued: bool = False) -> None:
        """Set the FlexFEC repair density from a measured loss fraction: one
        repair per group under 2% loss, two under 8%, three above, read on a
        smoothed loss so a single small window neither adds nor drops a
        repair on its own. Every group takes them while the path loses
        packets with no queue standing (`queued`), a loss a repair recovers
        without a retransmission's round trip: the loss read on windows
        FEC_CLEAR_WINDOWS past the last queue, since a queue's overflow shows
        in the windows it stood in and the ones just after. With nothing lost
        that way, or while a queue stands, whose overflow a repair per frame
        would only deepen, the repairs follow the media packets
        (`_fec_repairs`)."""
        self._fec_loss += (loss_fraction - self._fec_loss) * 0.3
        self._fec_clear = 0 if queued else self._fec_clear + 1
        if self._fec_clear > FEC_CLEAR_WINDOWS:
            self._fec_clear_loss += (loss_fraction - self._fec_clear_loss) * 0.3
        self.fec_repair_packets = 1 + (self._fec_loss > 0.02) + (self._fec_loss > 0.08)
        self._fec_full = self._fec_clear_loss > FEC_FULL_LOSS and not queued

    async def _next_encoded_frame(self) -> Optional[RTCEncodedFrame]:
        data = await self.__track.recv()

        # If the sender is disabled, drop the frame instead of packing it.
        # We still want to read from the track in order to avoid frames
        # accumulating in memory.
        if not self._enabled or self.__send_codec is None or self._peer_gone():
            return None
        # A frame the capture coded before a codec switch is not this codec's to pack.
        codec = getattr(data, "codec", None)
        if codec is not None and codec.lower() != self.__send_codec.mimeType.lower():
            return None

        if self.__encoder is None:
            self.__encoder = get_encoder(self.__send_codec)

        # Tracks serve frames pixelflux/pcmflux have already encoded; the sender
        # only packs them into RTP payloads. Keyframes are requested out of band
        # (the capture side produces the IDR), so no encode runs here.
        self.__force_keyframe_used = False
        try:
            payloads, timestamp, keyframe = self.__encoder.pack(data)
        except ValueError as e:
            # A frame its packer cannot read is dropped, not the sender with it.
            logger.warning("RTCRtpSender(%s) dropped a frame it cannot pack: %s", self.__kind, e)
            if self.__kind == "video":
                self._emit_pli_event()
            return None

        # If the packer did not return any payloads, return `None`.
        if not payloads:
            return None

        return RTCEncodedFrame(payloads, timestamp, None, data.keyframe, data.timing, data.dependency,
                               getattr(data, "anchor", False), data.data)

    def _describe(self, frame_id: int, reference: Optional[int], keyframe: bool) -> Optional[tuple]:
        """Number a frame on the wire and measure how far back it predicts, for its
        dependency descriptor: (number, frames back), the latter None for a frame that
        predicts from nothing. None for a frame predicting from one this sender never sent
        (a peer paused across it): undecodable here, so the caller leaves it out."""
        if keyframe:
            self.__frame_numbers.clear()
        fdiff = None
        if reference is not None:
            known = self.__frame_numbers.get(reference)
            if known is None:
                return None
            fdiff = (self.__frame_number - known) & 0xFFFF
            if not 1 <= fdiff <= 4096:
                return None
        number = self.__frame_number
        self.__frame_number = (number + 1) & 0xFFFF
        self.__frame_numbers[frame_id] = number
        if len(self.__frame_numbers) > FRAME_NUMBERS_MEMORY:
            del self.__frame_numbers[next(iter(self.__frame_numbers))]
        return number, fdiff

    async def _retransmit(self, packet: RtpPacket) -> bool:
        """Retransmit an RTP packet reported lost; False when the pacer dropped it."""
        media_seq = packet.sequence_number
        if self.__rtx_payload_type is not None:
            packet = wrap_rtx(
                packet,
                payload_type=self.__rtx_payload_type,
                sequence_number=self.__rtx_sequence_number,
                ssrc=self._rtx_ssrc,
            )
            self.__rtx_sequence_number = uint16_add(self.__rtx_sequence_number, 1)

        # A retransmission is a new packet on the wire: give it its own
        # transport-wide sequence number.
        packet.extensions.transport_sequence_number = self.transport._twcc_next(
            len(packet.payload)
        )
        if self.__unheld:
            self.__rtx_twcc[media_seq] = packet.extensions.transport_sequence_number
        self.__log_debug("> %s", packet)
        return await self._send(packet.serialize(self.__rtp_header_extensions_map),
                                packet.extensions.transport_sequence_number)

    async def _send(self, packet_bytes: bytes, twcc_seq: Optional[int] = None) -> bool:
        """Hand a packet to the transport. A video packet the pacer refuses was
        abandoned with its GOP, and every packet the history holds with it: NACKs
        for them go unanswered, since the keyframe the pacer asked for is their
        repair and a late retransmission would only land behind it."""
        sent = await self.transport._send_rtp(
            packet_bytes, rtc_class=CLASS_AUDIO if self.__kind == "audio" else CLASS_VIDEO,
            twcc_seq=twcc_seq)
        if not sent:
            self.__rtp_history.abandon()
        return sent

    def _send_keyframe(self) -> None:
        """
        Request the next frame to be a keyframe.
        """
        self.__force_keyframe = True

    def request_keyframe(self) -> None:
        """Public alias used by the transport pacer's GOP-reset recovery hook."""
        self._send_keyframe()

    async def _run_rtp(self) -> None:
        self.__log_debug("- RTP started")
        self.__rtp_started.set()
        if self.__kind == "video" and hasattr(self.transport, "set_video_resync"):
            self.transport.set_video_resync(self._pacer_resync)

        sequence_number = random_sequence_number()
        timestamp_origin = random32()
        # Timer-triggered video-timing diagnostics (~5 flagged frames/s, like libwebrtc).
        last_video_timing = 0.0
        last_abs_capture_ns = 0
        # FlexFEC group: serialized media packets awaiting their XOR repair
        # packets (flushed per frame, or every FEC_GROUP_PACKETS within a large frame).
        fec_group: list[bytes] = []
        fec_first_seq = 0
        try:
            while True:
                if not self.__track:
                    await asyncio.sleep(0.02)
                    continue

                # Fetch the next encoded frame. This can be `None` if the sender
                # is disabled, in which case we just continue the loop.
                enc_frame = await self._next_encoded_frame()
                if enc_frame is None:
                    # A frame this peer is not sent leaves its picture behind the screen.
                    self._frame_left = None
                    continue
                codec = self.__send_codec
                frame_time = time.time()
                instant_ns = (enc_frame.timing[0] if enc_frame.timing and enc_frame.timing[0] > 0
                              else time.monotonic_ns())

                if self.__kind == "video" and (
                    self.__force_keyframe_used or enc_frame.keyframe
                ):
                    # Report keyframe size to the pacer: feeds its IDR-aware
                    # queue budget and resurrects video after a GOP reset.
                    # Forced (recovery) keyframes resurrect but must not
                    # shrink the IDR floor. Every JPEG frame is one, so every
                    # one feeds the floor. Remember for late attach.
                    natural = not self.__force_keyframe_used
                    size = sum(len(p_) for p_ in enc_frame.payloads)
                    self._keyframe_bytes = size
                    self._keyframe_natural = natural
                    self.transport.note_video_keyframe(size, natural=natural)

                timestamp = uint32_add(timestamp_origin, enc_frame.timestamp)
                described = None
                standing_in = False
                descriptor = self.__rtp_header_extensions_map.has_dependency_descriptor()
                if enc_frame.dependency is not None and self._numbers_frames():
                    if not self._forward(*enc_frame.dependency, enc_frame.keyframe, enc_frame.anchor):
                        self._frame_left = None
                        if self.on_frame_left_out is not None:
                            self.on_frame_left_out()
                        stand_in = None if descriptor else self._stand_in(enc_frame)
                        if stand_in is None:
                            continue
                        enc_frame, standing_in = stand_in, True
                    else:
                        described = self._describe(*enc_frame.dependency, enc_frame.keyframe)
                        if described is None:
                            self._frame_left = None
                            self._undecodable(*enc_frame.dependency)
                            continue
                        if not descriptor and enc_frame.keyframe and self._numbered():
                            self._learn_parameter_sets(enc_frame)

                # abs-capture-time rides the first packet of a key frame and of a
                # frame a second after the last one, as libwebrtc paces it: the RTP
                # clock follows the capture, so a receiver extrapolates between them.
                abs_capture_time = None
                if self.__kind == "video" and (
                    enc_frame.keyframe or instant_ns - last_abs_capture_ns >= 1_000_000_000
                ):
                    last_abs_capture_ns = instant_ns
                    abs_capture_time = clock.ntp_from_monotonic_ns(instant_ns)

                # Every datagram of the frame is built before the first is sent:
                # building and sending in turn costs a third more per packet, as each
                # evicts what the other just warmed, and the frame is decodable only
                # once its last packet is in. Entries: bytes, transport-wide sequence
                # number, media sequence number (None for FlexFEC), payload length.
                outgoing: list[tuple[bytes, Optional[int], Optional[int], int]] = []
                # The first sequence number and count of each FlexFEC group it repairs.
                protected: list[tuple[int, int]] = []
                for i, payload in enumerate(enc_frame.payloads):
                    packet = RtpPacket(
                        payload_type=codec.payloadType,
                        sequence_number=sequence_number,
                        timestamp=timestamp,
                    )
                    packet.ssrc = self._ssrc
                    packet.payload = payload
                    packet.marker = (i == len(enc_frame.payloads) - 1) and 1 or 0

                    # set header extensions
                    packet.extensions.abs_send_time = (
                        clock.current_ntp_time() >> 14
                    ) & 0x00FFFFFF
                    packet.extensions.mid = self.__mid
                    packet.extensions.transport_sequence_number = (
                        self.transport._twcc_next(len(payload))
                    )
                    if enc_frame.audio_level is not None:
                        packet.extensions.audio_level = (False, -enc_frame.audio_level)
                    # https://webrtc.googlesource.com/src/+/main/docs/native-code/rtp-hdrext/playout-delay/README.md
                    # set min and max to 0 to hint the receiver to render frames as soon as possible
                    packet.extensions.playout_delay = (0, 0)
                    # The colour signal rides every packet of a key frame, as libwebrtc sends
                    # it; the receiver keeps it for the frames that follow.
                    color_space = RTP_COLOR_SPACE.get(codec.mimeType.lower())
                    if enc_frame.keyframe and color_space is not None:
                        packet.extensions.color_space = color_space
                    # video-timing rides the LAST packet of a frame, about five
                    # times a second like libwebrtc's timer-triggered frames.
                    if (
                        packet.marker
                        and self.__kind == "video"
                        and frame_time - last_video_timing >= 0.2
                    ):
                        last_video_timing = frame_time
                        arrival_ms = min(0xFFFF, max(0, int((time.time() - frame_time) * 1000)))
                        packet.extensions.video_timing = video_timing_legs(
                            enc_frame.timing, time.monotonic_ns(), arrival_ms)
                    if described is not None and descriptor:
                        packet.extensions.dependency_descriptor = dependency_descriptor(
                            i == 0, bool(packet.marker), described[0], described[1], enc_frame.keyframe)
                    if i == 0 and abs_capture_time is not None:
                        packet.extensions.abs_capture_time = abs_capture_time
                    self.__log_debug("> %s", packet)
                    self.__rtp_history.add(
                        packet, frame_time, enc_frame.dependency[0] if described is not None and descriptor else None)
                    packet_bytes = packet.serialize(self.__rtp_header_extensions_map)
                    outgoing.append((packet_bytes, packet.extensions.transport_sequence_number,
                                     packet.sequence_number, len(payload)))
                    sequence_number = uint16_add(sequence_number, 1)

                    if self.__fec_payload_type is not None:
                        if not fec_group:
                            fec_first_seq = packet.sequence_number
                        fec_group.append(
                            self.__rtp_header_extensions_map.for_fec(packet_bytes)
                        )
                        if packet.marker or len(fec_group) == FEC_GROUP_PACKETS:
                            repairs = self._fec_repairs(len(fec_group))
                            for repair in range(repairs):
                                fec_bytes = build_flexfec_03(
                                    fec_group,
                                    fec_first_seq,
                                    self._ssrc,
                                    self.__fec_payload_type,
                                    self.__fec_sequence_number,
                                    packet.timestamp,
                                    self._fec_ssrc,
                                    range(repair, len(fec_group), repairs),
                                )
                                self.__fec_sequence_number = uint16_add(
                                    self.__fec_sequence_number, 1
                                )
                                outgoing.append((fec_bytes, None, None, 0))
                            if repairs:
                                protected.append((fec_first_seq, len(fec_group)))
                            fec_group = []

                last = next((seq for _, seq, media, _ in reversed(outgoing)
                             if media is not None and seq is not None), None)
                # A stand-in is no frame of the stream's: the pacer counts no frame for it, and
                # the peer's frame was told left out already.
                if last is not None and self.__kind == "video" and not standing_in:
                    self._frame_handed = last
                    captured = enc_frame.timing[0] if enc_frame.timing else 0
                    self.transport.frame_end(last, self._frame_on_wire, last, enc_frame.keyframe,
                                             enc_frame.timing, captured,
                                             sum(len(p_) for p_ in enc_frame.payloads),
                                             enc_frame.dependency[0] if described is not None else None)
                    if described is not None:
                        self.__in_flight.append((last, enc_frame.dependency[0]))
                        if enc_frame.keyframe:
                            self._key_sent_at = time.monotonic()
                            self.__unheld.clear()
                            self.__held.clear()
                            self._unheld_gap = False
                            if self.on_key_sent is not None:
                                self.on_key_sent()
                        self.__unheld.append((
                            *enc_frame.dependency, enc_frame.keyframe, time.monotonic(),
                            [(seq, media) for _, seq, media, _ in outgoing if media is not None]))
                    if self.on_frame_queued is not None:
                        self.on_frame_queued()
                resyncs = self._resyncs
                for index, (packet_bytes, twcc_seq, media_seq, size) in enumerate(outgoing):
                    if self._resyncs != resyncs:
                        # A pacer reset cut this frame (`_pacer_resync`): the rest of it
                        # would only queue in front of the frame that resyncs the peer,
                        # and goes as the packets the pacer drops do.
                        for _, seq, _, _ in outgoing[index:]:
                            if seq is not None:
                                self.transport._twcc_dropped(seq)
                        if last is not None:
                            self.transport.forget_frame_end(last)
                        break
                    if media_seq is not None:
                        self.__last_sequence = media_seq
                    await self._send(packet_bytes, twcc_seq)
                    if media_seq is not None:
                        self.__octet_count += size
                        self.__packet_count += 1
                if protected:
                    repaired = time.time() + self.transport._send_delay()
                    for first, count in protected:
                        for offset in range(count):
                            self.__rtp_history.repaired(uint16_add(first, offset), repaired)
                self.__rtp_timestamp = timestamp
                self.__rtp_instant_ns = instant_ns
                self.__rtp_clock_rate = codec.clockRate
        except (asyncio.CancelledError, ConnectionError, MediaStreamError):
            pass
        except Exception:
            # we *need* to set __rtp_exited, otherwise RTCRtpSender.stop() will hang,
            # so issue a warning if we hit an unexpected exception
            self.__log_warning(traceback.format_exc())

        # stop track
        if self.__track:
            self.__track.stop()
            self.__track = None

        # release encoder
        self.__encoder = None

        self.__log_debug("- RTP finished")
        self.__rtp_exited.set()

    async def _run_rtcp(self) -> None:
        self.__log_debug("- RTCP started")
        self.__rtcp_started.set()

        try:
            while True:
                # The interval between RTCP packets is varied randomly over the
                # range [0.5, 1.5] times the calculated interval.
                await asyncio.sleep(0.5 + random.random())

                # RTCP SR
                ntp_timestamp, rtp_timestamp = self._sender_clock(time.monotonic_ns())
                packets: list[AnyRtcpPacket] = [
                    RtcpSrPacket(
                        ssrc=self._ssrc,
                        sender_info=RtcpSenderInfo(
                            ntp_timestamp=ntp_timestamp,
                            rtp_timestamp=rtp_timestamp,
                            packet_count=self.__packet_count & 0xFFFFFFFF,
                            octet_count=self.__octet_count & 0xFFFFFFFF,
                        ),
                    )
                ]
                self.__lsr = (ntp_timestamp >> 16) & 0xFFFFFFFF
                self.__lsr_time = time.time()

                # RTCP SDES
                if self.__cname is not None:
                    packets.append(
                        RtcpSdesPacket(
                            chunks=[
                                RtcpSourceInfo(
                                    ssrc=self._ssrc,
                                    items=[(1, self.__cname.encode("utf8"))],
                                )
                            ]
                        )
                    )

                # RTCP XR DLRR, after the SDES as libwebrtc sends it
                if self.__rrtrs:
                    packets.append(self._dlrr_report(time.monotonic_ns()))

                await self._send_rtcp(packets)
        except asyncio.CancelledError:
            pass

        # RTCP BYE
        packet = RtcpByePacket(sources=[self._ssrc])
        await self._send_rtcp([packet])

        self.__log_debug("- RTCP finished")
        self.__rtcp_exited.set()

    def _sender_clock(self, now_ns: int) -> tuple[int, int]:
        """The NTP timestamp of `now_ns` (CLOCK_MONOTONIC) and the RTP timestamp the
        media clock reads then, carried forward from the last frame sent (RFC 3550
        6.4.1): a report pairs the instant it is sent with the RTP clock at that
        instant, not with the last packet's timestamp, which stands for its capture.
        Zero before any frame, which a receiver takes for no mapping."""
        if not self.__rtp_instant_ns:
            return 0, 0
        elapsed = (now_ns - self.__rtp_instant_ns) * self.__rtp_clock_rate // 1_000_000_000
        return clock.ntp_from_monotonic_ns(now_ns), (self.__rtp_timestamp + elapsed) & 0xFFFFFFFF

    def _dlrr_report(self, now_ns: int) -> RtcpXrPacket:
        """The DLRR answering each reporter's last reference time, sent beside every sender
        report rather than once per reference time: libwebrtc drops the round trip it
        reports for a stream at a sender report arriving without one."""
        return RtcpXrPacket(
            ssrc=self._ssrc,
            dlrr=[
                (ssrc, lrr, (((now_ns - arrived_ns) << 16) // 1_000_000_000) & 0xFFFFFFFF)
                for ssrc, (lrr, arrived_ns) in self.__rrtrs.items()
            ],
        )

    async def _send_rtcp(self, packets: list[AnyRtcpPacket]) -> None:
        payload = b""
        for packet in packets:
            self.__log_debug("> %s", packet)
            payload += bytes(packet)

        try:
            await self.transport._send_rtp(payload)
        except ConnectionError:
            pass

    def __log_warning(self, msg: str, *args: object) -> None:
        logger.warning(f"RTCRtpsender(%s) {msg}", self.__kind, *args)
