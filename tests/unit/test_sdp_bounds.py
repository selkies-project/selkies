#!/usr/bin/env python3
"""A remote SDP is parsed in time linear in its length.

The parse runs on the event loop every session shares, on what a page sends
over signaling. A media section at the caps on codecs, feedback lines and
SSRCs parses at once; one past any cap is refused; a description far past
anything a browser sends is refused just as fast; and set_sdp refuses an
answer longer than its bound before parsing it.
"""
import asyncio
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
from selkies.webrtc import sdp
from selkies.webrtc_engine import MAX_REMOTE_SDP, RTCApp, RTCAppError

HEAD = ("v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\n"
        "a=fingerprint:sha-256 " + ":".join(["AA"] * 32) + "\r\na=setup:active\r\n"
        "a=ice-ufrag:abcd\r\na=ice-pwd:abcdefghijklmnopqrstuv\r\n")


def section(codecs: int, feedback: int, ssrcs: int) -> str:
    lines = ["m=video 9 UDP/TLS/RTP/SAVPF 96", "c=IN IP4 0.0.0.0", "a=mid:0", "a=rtcp-mux", "a=recvonly"]
    lines += ["a=rtpmap:96 VP8/90000"] * codecs
    lines += ["a=rtcp-fb:* nack"] * feedback
    lines += [f"a=ssrc:{1000 + i} cname:x" for i in range(ssrcs)]
    return HEAD + "\r\n".join(lines) + "\r\n"


def parse(text: str) -> tuple:
    start = time.monotonic()
    try:
        sdp.SessionDescription.parse(text)
        outcome = "parsed"
    except ValueError as e:
        outcome = f"refused: {e}"
    return outcome, time.monotonic() - start


def main() -> bool:
    res = H.Results("sdp-bounds")
    outcome, took = parse(section(sdp.MAX_CODECS, sdp.MAX_RTCP_FB, sdp.MAX_SSRCS))
    res.check("a section at every cap parses at once", outcome == "parsed" and took < 1.0, f"{outcome} {took:.3f} s")
    for label, args in (("codecs", (sdp.MAX_CODECS + 1, 0, 0)), ("feedback lines", (1, sdp.MAX_RTCP_FB + 1, 0)),
                        ("SSRCs", (1, 0, sdp.MAX_SSRCS + 1))):
        outcome, _ = parse(section(*args))
        res.check(f"one past the cap on {label} is refused", outcome.startswith("refused"), outcome)
    big = section(4000, 4000, 4000)
    outcome, took = parse(big)
    res.check(f"a {len(big) // 1024} KiB description of thousands is refused at once",
              outcome.startswith("refused") and took < 0.5, f"{outcome[:40]} {took:.3f} s")

    async def oversized() -> str:
        app = RTCApp(async_event_loop=asyncio.get_running_loop(), encoder="h264enc", stun_servers=[], turn_servers=[])
        try:
            await app.set_sdp("answer", "v=0\r\n" + "a=x\r\n" * (MAX_REMOTE_SDP // 5), "no-such-peer")
        except RTCAppError as e:
            return str(e)
        return "accepted"
    message = asyncio.run(oversized())
    res.check("set_sdp refuses an answer past its bound before anything else", "longer than" in message, message)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
