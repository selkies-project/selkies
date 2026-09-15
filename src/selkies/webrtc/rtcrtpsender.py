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
from collections.abc import Callable
from typing import Optional, Union


from . import clock, rtp
from .pacer import CLASS_AUDIO, CLASS_VIDEO
from .codecs import get_capabilities, get_encoder, is_rtx
from .codecs.base import Encoder
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
#: BT.709 primaries, transfer and matrix at limited range, with the BT.601 matrix for VP8,
#: which is held to the only one a keyframe header's single colour-space bit can name. A
#: receiver that reads the extension takes it over the bitstream, so H.264, H.265 and AV1 are
#: left to their own headers rather than told here — theirs carry the range as well, which a
#: 4:4:4 session signals as full and this table has no way to know.
RTP_COLOR_SPACE = {
    "video/vp8": (1, 1, 6, 1),
    "video/vp9": (1, 1, 1, 1),
}


class RTCEncodedFrame:
    def __init__(self, payloads: list[bytes], timestamp: int, audio_level: int,
                 keyframe: bool = False, timing: Optional[tuple] = None,
                 dependency: Optional[tuple] = None):
        self.payloads = payloads
        self.timestamp = timestamp
        self.audio_level = audio_level
        self.keyframe = keyframe
        self.timing = timing
        self.dependency = dependency


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
        self.__encoder: Optional[Encoder] = None
        # The negotiated codecs and the one frames go out as; None drops them.
        self.__codecs: list[RTCRtpCodecParameters] = []
        self.__send_codec: Optional[RTCRtpCodecParameters] = None
        self.__force_keyframe = False
        self.__force_keyframe_used = False
        # Last observed keyframe size (bytes) and whether it was a natural
        # IDR: lets a late-attaching pacer bootstrap its IDR floor from the
        # session-start keyframe instead of waiting for the next one.
        self._keyframe_bytes: Optional[int] = None
        self._keyframe_natural: bool = True
        self.__loop = asyncio.get_running_loop()
        self.__mid: Optional[str] = None
        self.__rtp_exited = asyncio.Event()
        self.__rtp_header_extensions_map = rtp.HeaderExtensionsMap()
        self.__rtp_started = asyncio.Event()
        self.__rtp_task: Optional[asyncio.Future[None]] = None
        self.__rtp_history = RtpHistory()
        # Frames numbered on the wire for the dependency descriptor, and the number each
        # capture frame id took, which a frame predicting from it is measured against.
        self.__frame_number = 0
        self.__frame_numbers: dict[int, int] = {}
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
        self.__started = False
        self.__stats = RTCStatsReport()
        self.__transport = transport

        # stats
        self.__lsr: Optional[int] = None
        self.__lsr_time: Optional[float] = None
        self.__ntp_timestamp = 0
        self.__rtp_timestamp = 0
        self.__octet_count = 0
        self.__packet_count = 0
        self.__rtt: Optional[float] = None

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
        now on: its payload type, its RTX type and its own packer. A codec the
        peer never took drops the frames instead, until a switch names one it
        did; before the sender starts, the answer settles the codec."""
        if not self.__started:
            return True
        codec = self.negotiated_codec(mime_type)
        self.__send_codec = codec
        self.__encoder = None
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
            lost = None
            for seq in packet.lost:
                sent, frame, times = self.__rtp_history.nacked(seq)
                if sent is None:
                    # Gone from the history: only a key frame brings the peer back.
                    self._emit_pli_event()
                    break
                await self._retransmit(sent)
                # A second NACK for the same packet says the retransmission did not reach
                # the peer either: the frame is lost to it, and the encoder is told so the
                # frames after it stop predicting from it.
                if times > 1 and frame is not None and frame != lost:
                    lost = frame
                    self.emit("lost_frame", frame)
        elif isinstance(packet, RtcpRtpfbPacket) and packet.fmt == RTCP_RTPFB_TWCC:
            self.transport._twcc_process_feedback(packet.fci)
        elif isinstance(packet, RtcpPsfbPacket) and packet.fmt == RTCP_PSFB_PLI:
            self._send_keyframe()
            self._emit_pli_event()
        elif isinstance(packet, RtcpPsfbPacket) and packet.fmt == RTCP_PSFB_FIR:
            # Full Intra Request (RFC 5104): same recovery as PLI — force a keyframe.
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

    def _emit_pli_event(self):
        """
        Emit a "pli" event to notify the application layer, which is
        responsible for instructing the encoder to generate that keyframe
        """
        self.emit("pli")

    def steer_fec(self, loss_fraction: float) -> None:
        """Set the FlexFEC repair density from a measured loss fraction: one
        repair per group under 2% loss, two under 8%, three above, read on a
        smoothed loss so a single small window neither adds nor drops a
        repair on its own."""
        self._fec_loss += (loss_fraction - self._fec_loss) * 0.3
        self.fec_repair_packets = 1 + (self._fec_loss > 0.02) + (self._fec_loss > 0.08)

    async def _next_encoded_frame(self) -> Optional[RTCEncodedFrame]:
        data = await self.__track.recv()

        # If the sender is disabled, drop the frame instead of packing it.
        # We still want to read from the track in order to avoid frames
        # accumulating in memory.
        if not self._enabled or self.__send_codec is None:
            return None

        if self.__encoder is None:
            self.__encoder = get_encoder(self.__send_codec)

        # Tracks serve frames pixelflux/pcmflux have already encoded; the sender
        # only packs them into RTP payloads. Keyframes are requested out of band
        # (the capture side produces the IDR), so no encode runs here.
        self.__force_keyframe_used = False
        payloads, timestamp, keyframe = self.__encoder.pack(data)

        # If the packer did not return any payloads, return `None`.
        if not payloads:
            return None

        return RTCEncodedFrame(payloads, timestamp, None, data.keyframe, data.timing, data.dependency)

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
        if len(self.__frame_numbers) > 64:
            del self.__frame_numbers[next(iter(self.__frame_numbers))]
        return number, fdiff

    async def _retransmit(self, packet: RtpPacket) -> None:
        """
        Retransmit an RTP packet which was reported as lost.
        """
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
        self.__log_debug("> %s", packet)
        packet_bytes = packet.serialize(self.__rtp_header_extensions_map)
        await self.transport._send_rtp(packet_bytes, rtc_class=CLASS_VIDEO)

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

        sequence_number = random_sequence_number()
        timestamp_origin = random32()
        # Timer-triggered video-timing diagnostics (~5 flagged frames/s, like libwebrtc).
        last_video_timing = 0.0
        # FlexFEC group: serialized media packets awaiting their XOR repair
        # packets (flushed per frame, or every 10 packets within a large frame).
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
                    continue
                codec = self.__send_codec
                frame_time = time.time()

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
                if enc_frame.dependency is not None and self.__rtp_header_extensions_map.has_dependency_descriptor():
                    described = self._describe(*enc_frame.dependency, enc_frame.keyframe)
                    if described is None:
                        # The frame predicts from one this peer was never sent, so
                        # nothing but a key frame decodes here.
                        logger.info("RTCRtpSender(%s) frame %s predicts from %s, which this peer "
                                    "was not sent; asking for a key frame",
                                    self.__kind, *enc_frame.dependency)
                        self._emit_pli_event()
                        continue

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
                    if described is not None:
                        packet.extensions.dependency_descriptor = dependency_descriptor(
                            i == 0, bool(packet.marker), described[0], described[1], enc_frame.keyframe)
                    # send packet
                    self.__log_debug("> %s", packet)
                    self.__rtp_history.add(
                        packet, frame_time, enc_frame.dependency[0] if described is not None else None)
                    packet_bytes = packet.serialize(self.__rtp_header_extensions_map)
                    await self.transport._send_rtp(
                        packet_bytes,
                        rtc_class=CLASS_AUDIO if self.__kind == "audio" else CLASS_VIDEO,
                    )

                    self.__ntp_timestamp = clock.current_ntp_time()
                    self.__rtp_timestamp = packet.timestamp
                    self.__octet_count += len(payload)
                    self.__packet_count += 1
                    sequence_number = uint16_add(sequence_number, 1)

                    if self.__fec_payload_type is not None:
                        if not fec_group:
                            fec_first_seq = packet.sequence_number
                        fec_group.append(packet_bytes)
                        if packet.marker or len(fec_group) == 10:
                            repairs = min(self.fec_repair_packets, len(fec_group))
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
                                await self.transport._send_rtp(fec_bytes, rtc_class=CLASS_VIDEO)
                            fec_group = []
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
                packets: list[AnyRtcpPacket] = [
                    RtcpSrPacket(
                        ssrc=self._ssrc,
                        sender_info=RtcpSenderInfo(
                            ntp_timestamp=self.__ntp_timestamp,
                            rtp_timestamp=self.__rtp_timestamp,
                            packet_count=self.__packet_count & 0xFFFFFFFF,
                            octet_count=self.__octet_count & 0xFFFFFFFF,
                        ),
                    )
                ]
                self.__lsr = ((self.__ntp_timestamp) >> 16) & 0xFFFFFFFF
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

                await self._send_rtcp(packets)
        except asyncio.CancelledError:
            pass

        # RTCP BYE
        packet = RtcpByePacket(sources=[self._ssrc])
        await self._send_rtcp([packet])

        self.__log_debug("- RTCP finished")
        self.__rtcp_exited.set()

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
