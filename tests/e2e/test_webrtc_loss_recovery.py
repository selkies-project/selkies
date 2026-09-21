#!/usr/bin/env python3
"""A lossy, delayed, rate-limited link recovers cleanly from every GOP the
pacer abandons: nothing of the abandoned GOP is delivered after the keyframe
that replaced it.

A real server in WebRTC mode streams a scene repainted every frame to a
headless client on the same vendored stack, with every ICE candidate on both
sides rewritten to a userspace relay that shapes, delays and drops packets
from a seeded stream, over each link in LINKS. The client's NACKs cross the
relay, so the server's retransmissions are real. Read at the client, per
video packet: its arrival, its sequence number, whether it repairs an earlier
one, and whether it opens a keyframe. Read from the server log: the GOP
resets, what each purged, and the pace at the end. The measures are the
keyframes the client saw, the repairs of a hole's packets that arrived behind
the keyframe closing the hole, how long a hole took to reach that keyframe,
and the longest gap between frames, against the resets that caused them.

Usage: python3 tests/e2e/test_webrtc_loss_recovery.py [bursts|narrow ...]
"""
import asyncio
import os
import sys
import time
from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "perf"))
import helpers as H  # noqa: E402
import test_pacer as rig  # noqa: E402
from selkies.webrtc.utils import uint16_gt, uint16_gte  # noqa: E402

WIDTH, HEIGHT = 1280, 720
PAINTER = os.path.join(H.TOOLS, "motion_scene.py")
MEASURE_S = float(os.environ.get("LOSS_WINDOW_S", "20"))


def opens_keyframe(head: bytes) -> bool:
    """Whether an H.264 payload starts a keyframe: an SPS or IDR NAL, on its own,
    first in an aggregate, or as the first fragment."""
    if not head:
        return False
    nal = head[0] & 0x1F
    if nal in (5, 7):
        return True
    if nal == 24 and len(head) >= 4:
        return (head[3] & 0x1F) in (5, 7)
    if nal == 28 and len(head) >= 2:
        return bool(head[1] & 0x80) and (head[1] & 0x1F) == 5
    return False


def analyze(packets: list, rtx_map: dict) -> dict:
    """What the client saw on the media stream and its repairs (FEC is left
    out): keyframes, repairs of a hole's packets arriving behind the keyframe
    that closed the hole, media behind a keyframe from before it, frame gaps,
    and how long each hole took to reach its keyframe."""
    streams = set(rtx_map) | set(rtx_map.values())
    keyframes: list = []
    stale_repairs = stale_media = repairs = 0
    frames: dict = {}
    # A hole is fifty or more media packets the client never saw in sequence:
    # [time before, first missing, last missing, time to the keyframe after].
    holes: list = []
    last: Optional[tuple] = None
    for t, ts, seq, osn, head, ssrc in packets:
        if streams and ssrc not in streams:
            continue
        if osn is not None:
            repairs += 1
            if any(h[3] is not None and uint16_gte(osn, h[1]) and uint16_gte(h[2], osn) for h in holes):
                stale_repairs += 1
            continue
        if ts not in frames and opens_keyframe(head):
            keyframes.append((t, seq))
            if holes and holes[-1][3] is None:
                holes[-1][3] = t - holes[-1][0]
        if keyframes and keyframes[-1][0] < t and uint16_gt(keyframes[-1][1], seq):
            stale_media += 1
        frames.setdefault(ts, t)
        if last is not None and uint16_gt(seq, last[1]) and (seq - last[1]) & 0xFFFF >= 50:
            holes.append([last[0], (last[1] + 1) & 0xFFFF, (seq - 1) & 0xFFFF, None])
        last = (t, seq)
    starts = sorted(frames.values())
    gaps = sorted(b - a for a, b in zip(starts, starts[1:]))
    recovered = sorted(1000 * h[3] for h in holes if h[3] is not None)
    return {"frames": len(frames), "keyframes": len(keyframes), "repairs": repairs,
            "stale_repairs": stale_repairs, "stale_media": stale_media, "holes": len(holes),
            "recovery_ms": (round(recovered[len(recovered) // 2]), round(recovered[-1])) if recovered else None,
            "gap_p99_ms": round(1000 * gaps[int(0.99 * (len(gaps) - 1))], 1) if gaps else None,
            "gap_max_ms": round(1000 * gaps[-1], 1) if gaps else None}


def server_counts(log_text: str) -> dict:
    resets = [line for line in log_text.splitlines() if "GOP reset" in line]
    purged = 0
    for line in resets:
        try:
            purged += int(line.split("=> GOP reset, ")[1].split(" ")[0])
        except (IndexError, ValueError):
            pass
    closed = [line for line in log_text.splitlines() if "pacer closed:" in line]
    pace = int(closed[-1].split("'pace_bps': ")[1].split(",")[0]) if closed else None
    return {"resets": len(resets), "purged": purged, "pace_bps": pace,
            "timeout_resurrects": log_text.count("resurrecting video optimistically")}


# Links: a wide one with a half-second blackout every four seconds, the burst a
# NACK batch of hundreds of packets answers, over a trickle of loss that keeps
# repairs flowing; and a narrow one the stream cannot fit, which cuts it all
# through the window and must brake the pace to what it carries.
LINKS = {
    "bursts": dict(rate_bps=6e6, delay_s=0.03, loss=0.01, seed=7, outage=(4.0, 0.5)),
    "narrow": dict(rate_bps=2.5e6, delay_s=0.03),
}


async def session(res: H.Results, log: str, link: str) -> Optional[dict]:
    feedback = {"nack": 0, "nacked": 0, "pli": 0}
    receiver = rig.rrx_mod.RTCRtpReceiver
    send_nack, send_pli = receiver._send_rtcp_nack, receiver._send_rtcp_pli

    async def counted_nack(self, ssrc, lost):
        feedback["nack"] += 1
        feedback["nacked"] += len(lost)
        await send_nack(self, ssrc, lost)

    async def counted_pli(self, ssrc):
        # A browser asks for a picture a few times a second at most; the stack's
        # own receiver would ask on every frame it cannot assemble.
        if time.monotonic() - feedback.get("pli_at", 0.0) < 0.4:
            return
        feedback["pli_at"] = time.monotonic()
        feedback["pli"] += 1
        await send_pli(self, ssrc)

    receiver._send_rtcp_nack, receiver._send_rtcp_pli = counted_nack, counted_pli
    rig.CURRENT_SHAPER["up"] = rig.Shaper(delay_s=0.03, loss=0.005, seed=11)
    rig.CURRENT_SHAPER["down"] = rig.Shaper(**LINKS[link])
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and "Registered peer server-" not in H.server_log(log):
        await asyncio.sleep(0.2)
    before = os.path.getsize(log)
    await rig.run_client(f"ws://127.0.0.1:{H.PORT}/api/ws", measure_s=MEASURE_S, warmup_s=8.0)
    seen = analyze(list(rig.REC.video_pkts), dict(rig.REC.rtx_map))
    with open(log, errors="replace") as f:
        f.seek(before)
        seen.update(server_counts(f.read()))
    seen["lost_down"] = rig.CURRENT_SHAPER["down"].lost + rig.CURRENT_SHAPER["down"].dropped
    seen.update({k: v for k, v in feedback.items() if k != "pli_at"})
    seen["streams"] = {ssrc: n for ssrc, n in rig.REC.ssrcs.items()}
    seen["rtx"] = dict(rig.REC.rtx_map)
    receiver._send_rtcp_nack, receiver._send_rtcp_pli = send_nack, send_pli
    return seen


def main() -> int:
    res = H.Results("webrtc-loss-recovery")
    xproc, H.TEST_DISPLAY = H.private_x_server(WIDTH, HEIGHT)
    painter = H.spawn([sys.executable, PAINTER, H.TEST_DISPLAY, str(WIDTH), str(HEIGHT), "60"])
    log = os.path.join(H.WORKDIR, "selkies-server.log")
    try:
        for link in (sys.argv[1:] or LINKS):
            H.server_start("webrtc", extra_env={
                "SELKIES_WEBRTC_PACER": "true", "SELKIES_STUN_HOST": "", "SELKIES_TURN_REST_URI": "",
                "SELKIES_VIDEO_BITRATE": "4000", "SELKIES_DEBUG": "true"}, log=log)
            seen = asyncio.run(session(res, log, link))
            H.server_stop()
            print(f"      {link}: {seen}")
            res.check(f"{link}: video crossed the link and the client saw keyframes",
                      seen["frames"] > (15 if link == "bursts" else 5) * MEASURE_S and seen["keyframes"] >= 1, seen)
            res.check(f"{link}: the link lost packets and the client had them repaired",
                      seen["lost_down"] > 0 and seen["repairs"] > 0, seen)
            res.check(f"{link}: the pacer abandoned at least one GOP",
                      seen["resets"] >= 1, seen)
            res.check(f"{link}: no hole's packet was repaired behind the keyframe that closed the hole",
                      seen["stale_repairs"] == 0, seen)
            res.check(f"{link}: no media from before a keyframe arrived behind it",
                      seen["stale_media"] == 0, seen)
            res.check(f"{link}: every abandoned GOP was replaced by a keyframe in time",
                      seen["timeout_resurrects"] == 0, seen)
            if link == "bursts":
                # The blackout is half a second; what follows it is the keyframe's
                # own way through the link, not a queue of abandoned video ahead of it.
                res.check("bursts: a hole closes within a few hundred milliseconds of the link coming back",
                          seen["recovery_ms"] is not None and seen["recovery_ms"][0] < 800, seen)
            else:
                # The pace climbs back a quarter per second after each brake, so
                # what it reads at the end is where that climb was, below the ceiling.
                res.check("narrow: loss spread through the window still sizes brakes",
                          seen["pace_bps"] is not None and seen["pace_bps"] < 10_000_000, seen)
    finally:
        H.server_stop()
        painter.terminate()
        H.stop_x_server(xproc, H.TEST_DISPLAY)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
