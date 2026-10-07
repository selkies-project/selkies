#!/usr/bin/env python3
"""A WebRTC sender switched to another codec sends only that codec's frames.

The sender takes the new codec's packer at the switch, and the capture restarts
behind it, so frames the old capture coded can still arrive. Each frame carries
the codec its pixelflux header names, and the sender drops one that is not its
codec's: a VP9 frame that reached the AV1 packer raised in it and ended the
sender's loop, and the peer saw no video again. A frame its packer cannot read
is dropped too, a key frame asked for, and the frames after it go out.
"""
import asyncio
import os
import sys
import time
from fractions import Fraction

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc import rtp  # noqa: E402
from selkies.webrtc.codecs.base import EncodedPacket  # noqa: E402
from selkies.webrtc.mediastreams import MediaStreamTrack  # noqa: E402
from selkies.webrtc.rtcrtpparameters import (  # noqa: E402
    RTCRtcpParameters,
    RTCRtpCodecParameters,
    RTCRtpSendParameters,
)
from selkies.webrtc.rtcrtpsender import RTCRtpSender  # noqa: E402
from selkies.webrtc_media_pipeline import MediaPipelinePixel  # noqa: E402

passed = failed = 0

VP9 = RTCRtpCodecParameters(mimeType="video/VP9", clockRate=90000, payloadType=98)
AV1 = RTCRtpCodecParameters(mimeType="video/AV1", clockRate=90000, payloadType=45)
# The start of a VP9 key frame. Read as AV1, its first byte is an OBU whose size runs past the end.
VP9_FRAME = bytes([0x82, 0x49, 0x83, 0x42, 0x00]) + b"\x5a" * 40
# A temporal delimiter and a key frame's OBU.
AV1_FRAME = b"\x12\x00" + b"\x32\x14\x10" + b"\x00" * 19


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [codec-switch] {label}  {detail}", flush=True)


class FakeTransport:
    """The DTLS transport surface RTCRtpSender drives, recording what it sends."""

    state = "connected"
    _stats_id = "transport"

    def __init__(self) -> None:
        self.rtp = []
        self.seq = 0

    def _register_rtp_sender(self, sender, parameters) -> None:
        pass

    def _unregister_rtp_sender(self, sender) -> None:
        pass

    def _peer_heard_at(self) -> float:
        return time.monotonic()

    def _twcc_next(self, size: int) -> int:
        self.seq = (self.seq + 1) & 0xFFFF
        return self.seq

    def note_video_keyframe(self, size: int, natural: bool = True) -> None:
        pass

    def frame_end(self, twcc_seq: int, sink, *args) -> None:
        sink(*args)

    async def _send_rtp(self, data: bytes, rtc_class=None, twcc_seq=None) -> bool:
        if not rtp.is_rtcp(data):
            self.rtp.append(rtp.RtpPacket.parse(data))
        return True


class QueueTrack(MediaStreamTrack):
    kind = "video"

    def __init__(self) -> None:
        super().__init__()
        self.queue: asyncio.Queue = asyncio.Queue()

    async def recv(self):
        return await self.queue.get()


def frame(data: bytes, pts: int, codec: str, key: bool = False) -> EncodedPacket:
    return EncodedPacket(data, pts, Fraction(1, 90000), key, None, None, codec)


async def switch() -> None:
    track = QueueTrack()
    transport = FakeTransport()
    sender = RTCRtpSender(track, transport)
    plis = []
    sender.on("pli", lambda: plis.append(True))
    sender.switch_codec("video/VP9")
    await sender.send(RTCRtpSendParameters(codecs=[VP9, AV1], muxId="0", rtcp=RTCRtcpParameters(cname="switch")))
    track.queue.put_nowait(frame(VP9_FRAME, 0, "video/VP9", key=True))
    await asyncio.sleep(0.05)
    sender.switch_codec("video/AV1")
    # The old capture's last frame, then the new one's first.
    track.queue.put_nowait(frame(VP9_FRAME, 1500, "video/VP9"))
    track.queue.put_nowait(frame(AV1_FRAME, 3000, "video/AV1", key=True))
    await asyncio.sleep(0.05)
    sent = [p.payload_type for p in transport.rtp]
    check("the old capture's frame after the switch is dropped, and the new codec's goes out",
          sent == [VP9.payloadType, AV1.payloadType], sent)
    check("no key frame is asked for a frame of the old codec", not plis, len(plis))
    track.queue.put_nowait(frame(VP9_FRAME, 4500, "video/AV1"))
    track.queue.put_nowait(frame(AV1_FRAME, 6000, "video/AV1", key=True))
    await asyncio.sleep(0.05)
    await sender.stop()
    sent = [p.payload_type for p in transport.rtp]
    check("a frame its packer cannot read is dropped, and the sender goes on",
          sent == [VP9.payloadType, AV1.payloadType, AV1.payloadType], sent)
    check("and a key frame is asked for in its place", len(plis) == 1, len(plis))


class Frame(bytes):
    """A pixelflux StripeFrame stand-in: the wire header, a payload and the stamps."""


def captured(type_byte: int, codec_byte: int) -> Frame:
    f = Frame(bytes([type_byte, codec_byte, 0, 1, 0, 0, 0x02, 0xD0, 0, 0, 0, 0]) + b"\x00" * 16)
    f.capture_ns = time.monotonic_ns()
    f.encode_start_ns = f.encode_end_ns = f.capture_ns
    f.reference_frame_id = -2
    f.frame_id = 1
    return f


def capture_tags() -> None:
    got = []
    pipeline = MediaPipelinePixel(async_event_loop=type("Loop", (), {"call_soon_threadsafe": lambda self, fn: fn()})(),
                                  encoder="h264enc", height=720)
    pipeline.produce_data = lambda buf, pts, kind, keyframe=True, timing=None, dependency=None, codec=None, \
        anchor=False: got.append(codec)
    for codec_id in range(6):
        pipeline._screen_capture_callback(captured(0x04, (codec_id << 4) | 0x01))
    pipeline._screen_capture_callback(captured(0x03, 0x00))
    check("each video frame carries the codec its header names, a JPEG stripe none",
          got == [None, "video/H264", "video/VP8", "video/VP9", "video/AV1", "video/H265", None], got)


capture_tags()
asyncio.run(switch())
print(f"[codec-switch] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
