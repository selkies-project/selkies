#!/usr/bin/env python3
"""A sender's repairs and key-frame requests on a path congestion control cut.

On a path that loses packets because it is full, every NACK naming a packet gone
from the history asked for a key frame (four a second through the display's
floor, each overflowing the path again), and retransmissions with FlexFEC took
several times the rate the path was steered to. Where congestion control
measures the path (`set_repair_budget`), repairs take at most REPAIR_SHARE of the
steered rate over REPAIR_WINDOW_S, and a sender's own request waits while a key
frame is asked of the encoder (KEY_ASK_S) or on the wire, neither acknowledged nor
past its round trip, KEY_MARGIN_S and its bytes' time at what the path delivers.
A peer's PLI waits the same way, FlexFEC leaves FEC_RESERVE of the budget to
retransmissions, a pacer reset clears the wait, and a sender congestion control
does not measure behaves as before. The congestion loop bounds repairs only under
a target a queue cut, not one loss alone cut (one tick, stand-ins). Driven with
stand-ins; no peer.
"""
import asyncio
import os
import sys
import time
from collections import deque
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

from selkies.webrtc.rtcrtpsender import (  # noqa: E402
    FEC_RESERVE, KEY_ASK_S, KEY_MARGIN_S, REPAIR_SHARE, REPAIR_WINDOW_S, RTCRtpSender, RtpHistory)
from selkies.webrtc.rtp import (  # noqa: E402
    RTCP_PSFB_PLI, RTCP_RTPFB_NACK, RtcpPsfbPacket, RtcpRtpfbPacket, RtpPacket)
from selkies import webrtc_mode  # noqa: E402

res = H.Results("repair-budget")


def sender(acked: set, events: list, sent: list):
    """A video sender stand-in with its real budget, gate and NACK handling, a history of packets 1-20
    (frames 10-13, 1000 bytes each) and a transport acknowledging the sequence numbers in `acked`."""
    history = RtpHistory()
    for seq in range(1, 21):
        history.add(RtpPacket(payload_type=96, sequence_number=seq, payload=b"x" * 1000), 0.0, 10 + (seq - 1) // 5)

    async def retransmit(packet):
        sent.append(packet.sequence_number)
        return True

    s = SimpleNamespace(
        _RTCRtpSender__rtp_history=history, _RTCRtpSender__rtt=0.1, _RTCRtpSender__in_flight=deque(),
        _retransmit=retransmit, _past_repair=lambda lost: False, _resync_held=lambda: False,
        _still_request=lambda: False, _send_keyframe=lambda: None, _frame_handed=None, _frame_left=None,
        _lost_at=None, on_frame_out=None, on_frame_sent=None, selective=None,
        transport=SimpleNamespace(_send_delay=lambda: 0.0, _twcc_acked=lambda seq: seq in acked),
        _emit_pli_event=lambda: events.append("key"), emit=lambda name, *args: events.append(name),
        _repair_bps=None, _delivery_bps=None, _key_asked=None, _key_wire=None, _repair_bytes=0.0, _repair_at=0.0)
    for name in ("set_repair_budget", "_repair_room", "_key_on_its_way", "_ask_key_frame", "_frame_on_wire",
                 "_pacer_resync"):
        setattr(s, name, getattr(RTCRtpSender, name).__get__(s))
    return s


def nack(*lost):
    return RtcpRtpfbPacket(fmt=RTCP_RTPFB_NACK, ssrc=1, media_ssrc=2, lost=list(lost))


PLI = RtcpPsfbPacket(fmt=RTCP_PSFB_PLI, ssrc=1, media_ssrc=2)
GONE = 9999


async def main() -> None:
    acked, events, sent = set(), [], []
    s = sender(acked, events, sent)

    res.check("unmeasured, a repair of any size fits", s._repair_room(10_000_000))
    for _ in range(3):
        await RTCRtpSender._handle_rtcp_packet(s, nack(GONE))
    res.check("unmeasured, each NACK naming a packet gone asks for a key frame, as before",
              events.count("key") == 3, events)

    s.set_repair_budget(800_000, 1_000_000)
    room = 800_000 * REPAIR_SHARE / 8 * REPAIR_WINDOW_S
    res.check("measured, the budget holds REPAIR_WINDOW_S of REPAIR_SHARE of the steered rate",
              s._repair_room(room) and not s._repair_room(1), (room, s._repair_bytes))
    s._repair_at -= 0.1
    res.check("and refills at that rate", s._repair_room(room * 0.1 / REPAIR_WINDOW_S * 0.9)
              and not s._repair_room(room * 0.1 / REPAIR_WINDOW_S * 0.2))

    sent.clear()
    s._repair_bytes, s._repair_at = 4500.0, time.monotonic()
    await RTCRtpSender._handle_rtcp_packet(s, nack(1, 2, 3, 4))
    res.check("a NACK's retransmissions stop at the budget, a first repair counting twice (it is sent twice)",
              sent == [1, 1, 2, 2], sent)
    s._repair_bytes = room
    await RTCRtpSender._handle_rtcp_packet(s, nack(3, 4))
    res.check("and resume once it has room again; a packet the budget refused counted its NACK, so the next "
              "sends it once (and names its frame lost)", sent[-3:] == [3, 4, 4] and "lost_frame" in events,
              (sent, events))

    events.clear()
    s._repair_bytes, s._key_asked = room, None
    await RTCRtpSender._handle_rtcp_packet(s, nack(GONE))
    await RTCRtpSender._handle_rtcp_packet(s, nack(GONE))
    res.check("measured, a NACK naming a packet gone asks once, and the next waits on the key frame asked",
              events.count("key") == 1, events)
    s._key_asked -= KEY_ASK_S
    await RTCRtpSender._handle_rtcp_packet(s, nack(GONE))
    res.check("past KEY_ASK_S with no key frame out, it asks again", events.count("key") == 2, events)

    s._frame_on_wire(40, True, None, 0, 70_000)
    await RTCRtpSender._handle_rtcp_packet(s, nack(GONE))
    res.check("a key frame on the wire answers it: no request", events.count("key") == 2, events)
    travel = 0.1 + KEY_MARGIN_S + 70_000 * 8 / 1_000_000
    s._key_wire = (40, time.monotonic() - travel + 0.05, 70_000)
    await RTCRtpSender._handle_rtcp_packet(s, nack(GONE))
    res.check("until its bytes' time at what the path delivers, the round trip and KEY_MARGIN_S",
              events.count("key") == 2, events)
    s._key_wire = (40, time.monotonic() - travel - 0.05, 70_000)
    await RTCRtpSender._handle_rtcp_packet(s, nack(GONE))
    res.check("past that it asks again", events.count("key") == 3, events)

    s._frame_on_wire(41, True, None, 0, 70_000)
    acked.add(41)
    await RTCRtpSender._handle_rtcp_packet(s, nack(GONE))
    res.check("a key frame the peer acknowledged answers nothing new: it asks", events.count("key") == 4, events)

    s._frame_on_wire(42, True, None, 0, 70_000)
    await RTCRtpSender._handle_rtcp_packet(s, PLI)
    res.check("a peer's PLI waits on a key frame on its way too (it repeats while one crosses a slow path)",
              events.count("key") == 4, events)

    s._frame_on_wire(43, True, None, 0, 70_000)
    s._pacer_resync(None)
    await RTCRtpSender._handle_rtcp_packet(s, nack(GONE))
    res.check("a pacer reset drops the key frame on its way: the next request asks",
              events.count("key") == 5 and s._key_wire is None, events)
    s._frame_on_wire(44, True, None, 0, 70_000)
    s._delivery_bps = None
    await RTCRtpSender._handle_rtcp_packet(s, PLI)
    res.check("unmeasured, a peer's PLI is answered whatever is on its way, as before", events.count("key") == 6, events)

    s.set_repair_budget(800_000, 1_000_000)
    s._repair_bytes, s._repair_at = room * (FEC_RESERVE + 0.01), time.monotonic()
    res.check("FlexFEC leaves FEC_RESERVE of the budget to retransmissions",
              not s._repair_room(room * 0.02, proactive=True) and s._repair_room(room * 0.02))

    s.set_repair_budget(None)
    res.check("a budget lifted lets any repair through again", s._repair_room(10_000_000))




async def budgeted(capacity_kbps) -> list:
    """One congestion loop tick steering a display to 800 kbit/s over a path that delivered 500, the steer holding
    `capacity_kbps` (a queue it measured) or None (a cut by loss alone): what the video sender's budget was set to."""
    budgets = []
    steer = SimpleNamespace(hold_until=0.0, restored=False, capacity_kbps=capacity_kbps,
                            target=lambda *a, **k: 800.0)
    pipeline = SimpleNamespace(rc_mode=webrtc_mode.RateControlMode.CBR, video_bitrate=1000,
                               set_video_bitrate=AsyncMock())
    service = webrtc_mode.WebRTCService.__new__(webrtc_mode.WebRTCService)
    service.metrics, service.args = None, SimpleNamespace(congestion_control=True)
    service._congestion_steer = {"primary": steer}
    service._path_probes, service._padding, service._rate_holds, service._link_allowances = {}, {}, {}, {}
    service._audio_sent = {}
    service._display_setting = lambda did, key: 6000
    service.display_pipelines = {"primary": pipeline}
    service._ensure_pacer = Mock()
    service._display_owner_tabs, service._peer_tabs = {}, {}
    window = dict(goodput_bps=500_000, sent_bps=500_000, loss_fraction=0.1, queue_ms=0.0,
                  queue_rising_ms=None, queue_depth_ms=0.0)
    transport = SimpleNamespace(take_twcc_window=lambda: dict(window))
    video = SimpleNamespace(set_repair_budget=lambda bps, delivery: budgets.append((bps, delivery)),
                            steer_fec=lambda *a: None)
    service.rtc_app = SimpleNamespace(peer_connections={"peer": dict(
        peer_conn=SimpleNamespace(sctp=SimpleNamespace(transport=transport)), display_id="primary",
        video_sender=video)}, send_cc_rate=lambda did, kbps: None, set_video_budget=lambda did, bps: None)
    sleep = AsyncMock(side_effect=[None, asyncio.CancelledError()])
    with patch.object(webrtc_mode, "asyncio", SimpleNamespace(sleep=sleep)), \
            patch.object(webrtc_mode, "time", SimpleNamespace(monotonic=lambda: 32.0)), \
            patch.object(webrtc_mode, "settings", SimpleNamespace(video_bitrate=(100, 12000), webrtc_pacer=(True,))):
        try:
            await service._congestion_control_loop()
        except asyncio.CancelledError:
            pass
    return budgets


asyncio.run(main())
budgets = asyncio.run(budgeted(1000.0))
res.check("a target cut by a queue the path showed bounds the video sender's repairs by it",
          budgets == [(800_000, 500_000)], budgets)
budgets = asyncio.run(budgeted(None))
res.check("one cut by loss alone leaves them unbound (a random loss's repairs crowd nothing), the key frame "
          "still timed by the delivery", budgets == [(None, 500_000)], budgets)
sys.exit(0 if res.summary() else 1)
