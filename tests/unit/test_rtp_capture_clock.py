#!/usr/bin/env python3
"""The WebRTC video RTP clock follows the capture.

The pipeline takes each frame's 90 kHz pts from the instant pixelflux captured it,
so the encode's varying duration never enters the RTP clock, and pts keeps rising
across capture restarts and frame-rate changes. The sender's reports pair the NTP
time they are sent at with the RTP clock at that same instant, so a receiver
mapping a frame's RTP timestamp through them reads its capture time to well under
a millisecond; and a peer that negotiated abs-capture-time is told a key frame's
capture instant, and a frame's about once a second, on the NTP clock of those
reports.
"""
import asyncio
import os
import sys
import time
from fractions import Fraction

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc import clock, rtp  # noqa: E402
from selkies.webrtc.codecs import HEADER_EXTENSIONS  # noqa: E402
from selkies.webrtc.codecs.base import EncodedPacket  # noqa: E402
from selkies.webrtc.mediastreams import MediaStreamTrack  # noqa: E402
from selkies.webrtc.rtcrtpparameters import (  # noqa: E402
    RTCRtcpParameters,
    RTCRtpCodecParameters,
    RTCRtpHeaderExtensionParameters,
    RTCRtpSendParameters,
)
from selkies.webrtc.rtcrtpsender import RTCRtpSender  # noqa: E402
from selkies.webrtc_media_pipeline import MediaPipelinePixel  # noqa: E402

passed = failed = 0
MS = 1_000_000
ABS_CAPTURE_TIME = "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time"


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [capture-clock] {label}  {detail}", flush=True)


class Frame(bytes):
    """A pixelflux StripeFrame stand-in: the wire header plus the stamps."""


def frame(fid: int, capture_ns: int, encode_ms: float = 4.0, key: bool = False) -> Frame:
    header = bytes([0x04, 0x01 if key else 0x00, (fid >> 8) & 0xFF, fid & 0xFF, 0, 0, 0x02, 0xD0, 0, 0, 0, 0])
    f = Frame(header + b"\x00\x00\x00\x01\x65" + b"\x88" * 16)
    f.capture_ns = capture_ns
    f.encode_start_ns = capture_ns + MS if capture_ns else 0
    f.encode_end_ns = capture_ns + int(encode_ms * MS) if capture_ns else 0
    f.reference_frame_id = -2
    f.stripe_y_start = 0
    f.stripe_height = 720
    f.frame_id = fid
    return f


class ImmediateLoop:
    """Runs what the capture thread hands the loop at once."""

    def call_soon_threadsafe(self, fn, *args):
        fn(*args)


def pipeline_pts() -> None:
    p = MediaPipelinePixel(async_event_loop=ImmediateLoop(), encoder="h264enc", height=720)
    got = []
    p.produce_data = lambda buf, pts, kind, keyframe=True, timing=None, dependency=None: got.append((pts, timing))
    base = 1_000_000 * MS
    # 60 fps with an encode time that swings between 2 and 14 ms: only the capture counts.
    for i, enc in enumerate((2, 14, 3, 11, 2)):
        p._screen_capture_callback(frame(i, base + i * 16_666_667, enc, key=i == 0))
    steps = [b[0] - a[0] for a, b in zip(got, got[1:])]
    check("pts steps follow the capture, not the encode", steps == [1500] * 4, steps)
    check("the frame's timing leads with the capture instant its pts was taken at",
          all(t[0] == base + i * 16_666_667 for i, (_, t) in enumerate(got)), [t[0] for _, t in got])
    # A restart: frame ids begin again at 0 and the capture clock runs on, 250 ms later.
    last = got[-1][0]
    restart = base + 4 * 16_666_667 + 250 * MS
    p._screen_capture_callback(frame(0, restart, key=True))
    check("a restart continues pts by the capture gap", got[-1][0] - last == 22_500, got[-1][0] - last)
    # A live change to 30 fps, then 120 fps.
    t = restart
    for dt in (33_333_333, 33_333_333, 8_333_333, 8_333_333):
        t += dt
        p._screen_capture_callback(frame(1, t))
    steps = [b[0] - a[0] for a, b in zip(got[-5:], got[-4:])]
    check("frame-rate changes step pts by the new interval", steps == [3000, 3000, 750, 750], steps)
    # A frame stamped at or before the last one still rises by one tick.
    before = got[-1][0]
    p._screen_capture_callback(frame(2, t))
    p._screen_capture_callback(frame(3, t - MS))
    check("a tie or a step back bumps one tick", [got[-2][0], got[-1][0]] == [before + 1, before + 2],
          [got[-2][0] - before, got[-1][0] - before])
    # A path that stamps no capture falls back to the delivery, and says so in the timing.
    t0 = time.monotonic_ns()
    p._screen_capture_callback(frame(4, 0))
    t1 = time.monotonic_ns()
    pts, timing = got[-1]
    check("an unstamped frame takes its delivery instant", t0 <= timing[0] <= t1 and pts > before + 2,
          (timing[0] - t0, pts - before))


class FakeTransport:
    """The DTLS transport surface RTCRtpSender drives, recording what it sends."""

    state = "connected"
    _stats_id = "transport"

    def __init__(self) -> None:
        self.rtp = []
        self.rtcp = []
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

    async def _send_rtp(self, data: bytes, rtc_class=None, twcc_seq=None) -> bool:
        if rtp.is_rtcp(data):
            self.rtcp.append((time.monotonic_ns(), data))
        else:
            self.rtp.append((time.monotonic_ns(), data))
        return True


class QueueTrack(MediaStreamTrack):
    kind = "video"

    def __init__(self) -> None:
        super().__init__()
        self.queue: asyncio.Queue = asyncio.Queue()

    async def recv(self):
        return await self.queue.get()


def send_parameters(with_abs_capture_time: bool) -> RTCRtpSendParameters:
    codec = RTCRtpCodecParameters(mimeType="video/H264", clockRate=90000, payloadType=102,
                                  parameters={"packetization-mode": "1", "profile-level-id": "42e01f"})
    extensions = [e for e in HEADER_EXTENSIONS["video"] if with_abs_capture_time or e.uri != ABS_CAPTURE_TIME]
    return RTCRtpSendParameters(codecs=[codec], headerExtensions=extensions, muxId="0",
                                rtcp=RTCRtcpParameters(cname="capture-clock"))


def wall_ntp(monotonic_ns: int) -> float:
    """The NTP seconds of a CLOCK_MONOTONIC instant, read off the wall clock now."""
    return (monotonic_ns - time.monotonic_ns()) / 1e9 + time.time() + clock.NTP_UNIX_OFFSET


async def sender_reports() -> None:
    track = QueueTrack()
    transport = FakeTransport()
    sender = RTCRtpSender(track, transport)
    await sender.send(send_parameters(True))
    ext_map = rtp.HeaderExtensionsMap()
    ext_map.configure(send_parameters(True))
    captures = {}
    start = time.monotonic_ns()
    # 60 fps for 2.2 s; each frame reaches the sender 25 ms after its capture, as an
    # encode and a hop onto the loop would deliver it.
    for i in range(132):
        capture = start + i * 16_666_667
        await asyncio.sleep(max(0.0, (capture + 25 * MS - time.monotonic_ns()) / 1e9))
        pts = (capture - start) * 9 // 100_000
        captures[pts] = capture
        track.queue.put_nowait(EncodedPacket(b"\x00\x00\x00\x01\x65" + b"\x88" * 40, pts, Fraction(1, 90000),
                                             i == 0, (capture, capture + MS, capture + 20 * MS), None))
    await asyncio.sleep(1.6)
    await sender.stop()

    packets = [(t, rtp.RtpPacket.parse(d, ext_map)) for t, d in transport.rtp]
    origin = (packets[0][1].timestamp - 0) & 0xFFFFFFFF
    reports = [p for _, d in transport.rtcp for p in rtp.RtcpPacket.parse(d) if isinstance(p, rtp.RtcpSrPacket)]
    worst = 0.0
    for sr in reports:
        if not sr.sender_info.ntp_timestamp:
            continue
        ntp = sr.sender_info.ntp_timestamp / (1 << 32)
        for _, packet in packets:
            offset = (sr.sender_info.rtp_timestamp - packet.timestamp) & 0xFFFFFFFF
            if offset >= 1 << 31:
                offset -= 1 << 32
            estimate = ntp - offset / 90000
            capture = captures[(packet.timestamp - origin) & 0xFFFFFFFF]
            worst = max(worst, abs(estimate - wall_ntp(capture)))
    check("sender reports went out", len(reports) >= 2, len(reports))
    check("a report maps every frame's RTP timestamp to its capture within 1 ms", worst < 0.001,
          f"worst {worst * 1000:.3f} ms")

    firsts = {}
    for _, packet in packets:
        firsts.setdefault(packet.timestamp, packet)
    stamped = [p for p in firsts.values() if p.extensions.abs_capture_time is not None]
    check("abs-capture-time on the key frame and about once a second",
          2 <= len(stamped) <= 4 and stamped[0].timestamp == packets[0][1].timestamp, len(stamped))
    error = max(abs(p.extensions.abs_capture_time / (1 << 32)
                    - wall_ntp(captures[(p.timestamp - origin) & 0xFFFFFFFF])) for p in stamped)
    check("abs-capture-time is the capture instant on the reports' NTP clock", error < 0.001,
          f"{error * 1e6:.1f} us")
    others = [p for _, p in packets if p.extensions.abs_capture_time is not None and firsts[p.timestamp] is not p]
    check("only a frame's first packet carries it", not others, len(others))


async def not_negotiated() -> None:
    track = QueueTrack()
    transport = FakeTransport()
    sender = RTCRtpSender(track, transport)
    await sender.send(send_parameters(False))
    now = time.monotonic_ns()
    track.queue.put_nowait(EncodedPacket(b"\x00\x00\x00\x01\x65" + b"\x88" * 40, 0, Fraction(1, 90000),
                                         True, (now, now, now), None))
    await asyncio.sleep(0.05)
    await sender.stop()
    ext_map = rtp.HeaderExtensionsMap()
    ext_map.configure(send_parameters(True))
    parsed = [rtp.RtpPacket.parse(d, ext_map) for _, d in transport.rtp]
    check("a peer that did not negotiate it is sent none",
          parsed and all(p.extensions.abs_capture_time is None for p in parsed), len(parsed))


def offer() -> None:
    video = [e for e in HEADER_EXTENSIONS["video"] if e.uri == ABS_CAPTURE_TIME]
    ids = [e.id for kind in HEADER_EXTENSIONS.values() for e in kind if e.uri != ABS_CAPTURE_TIME]
    check("offered on video, with an id no other extension holds", len(video) == 1 and video[0].id not in ids,
          [e.id for e in video])
    check("not offered on audio", not any(e.uri == ABS_CAPTURE_TIME for e in HEADER_EXTENSIONS["audio"]))
    ext_map = rtp.HeaderExtensionsMap()
    ext_map.configure(RTCRtpSendParameters(headerExtensions=[RTCRtpHeaderExtensionParameters(id=9, uri=ABS_CAPTURE_TIME)]))
    packet = rtp.RtpPacket(payload_type=102, sequence_number=1, timestamp=0)
    packet.payload = b"\x00"
    packet.extensions.abs_capture_time = 0xE9C2_1234_8000_0000
    data = packet.serialize(ext_map)
    check("8-byte wire form round-trips", b"\x97\xe9\xc2\x12\x34\x80\x00\x00\x00" in data
          and rtp.RtpPacket.parse(data, ext_map).extensions.abs_capture_time == 0xE9C2_1234_8000_0000)
    a = clock.ntp_from_monotonic_ns(10**15)
    b = clock.ntp_from_monotonic_ns(10**15 + 1_500_000_000)
    check("the NTP clock runs with CLOCK_MONOTONIC", abs((b - a) / (1 << 32) - 1.5) < 1e-8, (b - a) / (1 << 32))


pipeline_pts()
offer()
asyncio.run(sender_reports())
asyncio.run(not_negotiated())
print(f"[capture-clock] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
