#!/usr/bin/env python3
"""A receive-only peer learns its round trip from the sender's RTCP XR answers.

A libwebrtc receiver that has no stream of its own to send reports through sends a
Receiver Reference Time Report (RFC 3611 block type 4) once the offer carried
`rtcp-fb rrtr`; the sender answers every reporter's last one with a DLRR sub-block
(block type 5) beside each sender report, from which the receiver computes the
round trip it needs to place a frame's capture time on its own clock. Checks the
wire layout of both blocks, their parse inside a compound packet, the routing of a
reference time to every sender, the delay a sender reports, and the offer.
"""
import asyncio
import os
import struct
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc import rtp  # noqa: E402
from selkies.webrtc.codecs import CODECS  # noqa: E402
from selkies.webrtc.mediastreams import MediaStreamTrack  # noqa: E402
from selkies.webrtc.rtcdtlstransport import RtpRouter  # noqa: E402
from selkies.webrtc.rtcrtpparameters import (  # noqa: E402
    RTCRtcpParameters,
    RTCRtpCodecParameters,
    RTCRtpSendParameters,
)
from selkies.webrtc.rtcrtpsender import RTCRtpSender  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [rtcp-xr] {label}  {detail}", flush=True)


class FakeTransport:
    """The DTLS transport surface RTCRtpSender drives, recording the RTCP it sends."""

    state = "connected"
    _stats_id = "transport"

    def __init__(self) -> None:
        self.rtcp = []

    def _register_rtp_sender(self, sender, parameters) -> None:
        pass

    def _unregister_rtp_sender(self, sender) -> None:
        pass

    async def _send_rtp(self, data: bytes, rtc_class=None, twcc_seq=None) -> bool:
        if rtp.is_rtcp(data):
            self.rtcp.append(data)
        return True


class IdleTrack(MediaStreamTrack):
    kind = "video"

    async def recv(self):
        await asyncio.Event().wait()


def layout() -> None:
    rrtr = bytes(rtp.RtcpXrPacket(ssrc=0x11223344, rrtr=0xE9C2123480000000))
    check("RRTR: XR header, reporter SSRC, block type 4 of length 2, 64-bit NTP time",
          rrtr == bytes.fromhex("80cf0004" "11223344" "04000002" "e9c2123480000000"), rrtr.hex())
    dlrr = bytes(rtp.RtcpXrPacket(ssrc=0xAABBCCDD, dlrr=[(1, 0x12348000, 0x4000), (0x55667788, 7, 9)]))
    check("DLRR: block type 5 of length 3 per sub-block, each SSRC, LRR, DLRR",
          dlrr == bytes.fromhex("80cf0008" "aabbccdd" "05000006" "00000001" "12348000" "00004000"
                                "55667788" "00000007" "00000009"), dlrr.hex())
    check("an XR without blocks is its header and SSRC", bytes(rtp.RtcpXrPacket(ssrc=5)) == bytes.fromhex("80cf000100000005"))


def parse() -> None:
    sr = rtp.RtcpSrPacket(ssrc=9, sender_info=rtp.RtcpSenderInfo(ntp_timestamp=1, rtp_timestamp=2, packet_count=3,
                                                                  octet_count=4))
    voip = struct.pack("!BBH", 7, 0, 8) + b"\x00" * 32
    unknown = rtp.pack_rtcp_packet(rtp.RTCP_XR, 0, struct.pack("!L", 1) + voip
                                   + struct.pack("!BBHQ", rtp.RTCP_XR_RRTR, 0, 2, 0x0102030405060708))
    rr = rtp.RtcpRrPacket(ssrc=1)
    packets = rtp.RtcpPacket.parse(bytes(rr) + unknown + bytes(sr)
                                   + bytes(rtp.RtcpXrPacket(ssrc=9, dlrr=[(1, 2, 3), (4, 5, 6)])))
    check("a compound packet parses around its extended reports", [type(p).__name__ for p in packets]
          == ["RtcpRrPacket", "RtcpXrPacket", "RtcpSrPacket", "RtcpXrPacket"], packets)
    check("a block type it does not read is skipped, the RRTR after it read",
          packets[1] == rtp.RtcpXrPacket(ssrc=1, rrtr=0x0102030405060708), packets[1])
    check("every DLRR sub-block is read", packets[3].dlrr == [(1, 2, 3), (4, 5, 6)], packets[3].dlrr)
    truncated = rtp.pack_rtcp_packet(rtp.RTCP_XR, 0, struct.pack("!LBBH", 1, rtp.RTCP_XR_DLRR, 0, 6) + b"\x00" * 12)
    try:
        rtp.RtcpPacket.parse(truncated)
        check("a block running past its packet is refused", False)
    except ValueError as exc:
        check("a block running past its packet is refused", True, exc)


def routing() -> None:
    router = RtpRouter()
    video, audio = object(), object()
    router.register_sender(video, 100)
    router.register_sender(audio, 200)
    check("a reference time reaches every sender",
          router.route_rtcp(rtp.RtcpXrPacket(ssrc=1, rrtr=5)) == {video, audio})
    check("an XR without one reaches none", router.route_rtcp(rtp.RtcpXrPacket(ssrc=1, dlrr=[(100, 1, 2)])) == set())


async def answer() -> None:
    transport = FakeTransport()
    sender = RTCRtpSender(IdleTrack(), transport)
    codec = RTCRtpCodecParameters(mimeType="video/H264", clockRate=90000, payloadType=102)
    await sender.send(RTCRtpSendParameters(codecs=[codec], rtcp=RTCRtcpParameters(cname="xr")))
    check("no DLRR before a reference time", not any(isinstance(p, rtp.RtcpXrPacket)
                                                     for p in rtp.RtcpPacket.parse(b"".join(transport.rtcp))))
    ntp = 0xE9C2123456789ABC
    await sender._handle_rtcp_packet(rtp.RtcpXrPacket(ssrc=1, rrtr=ntp))
    arrived = time.monotonic_ns()
    report = sender._dlrr_report(arrived + 250_000_000)
    (ssrc, lrr, delay), = report.dlrr
    check("the DLRR names the reporter and the middle 32 bits of its time", (ssrc, lrr) == (1, 0x12345678),
          (ssrc, hex(lrr)))
    check("the delay since it arrived is in 1/65536 s", abs(delay - 16384) <= 2, delay)
    check("the DLRR is the sender's own", report.ssrc == sender._ssrc)
    await sender._handle_rtcp_packet(rtp.RtcpXrPacket(ssrc=1, rrtr=ntp + (1 << 32)))
    check("a newer reference time replaces the reporter's last", len(sender._dlrr_report(arrived).dlrr) == 1)
    await asyncio.sleep(1.6)
    await sender.stop()
    compounds = [rtp.RtcpPacket.parse(d) for d in transport.rtcp]
    reports = [c for c in compounds if isinstance(c[0], rtp.RtcpSrPacket)]
    check("every sender report carries the DLRR", reports and all(
        any(isinstance(p, rtp.RtcpXrPacket) and p.dlrr and p.dlrr[0][1] == 0x12355678 for p in c)
        for c in reports), len(reports))


def offer() -> None:
    video = [c for c in CODECS["video"] if c.mimeType.lower() not in ("video/rtx", "video/flexfec-03")]
    check("every video media codec offers rrtr", video and all(any(f.type == "rrtr" for f in c.rtcpFeedback) for c in video))
    check("no audio codec does", not any(f.type == "rrtr" for c in CODECS["audio"] for f in c.rtcpFeedback))


layout()
parse()
routing()
asyncio.run(answer())
offer()
print(f"[rtcp-xr] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
