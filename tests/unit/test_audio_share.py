#!/usr/bin/env python3
"""Audio's share of a congested path.

Transport-cc counts a peer's audio in what the path delivered, so congestion
control reads the audio sender's rate (`_audio_bps`) to steer the video by what
is left (one loop tick, stand-ins). While the video target is cut and the audio takes more than
AUDIO_SHARE of the delivery, its RED depth drops a payload a tick, and comes
back one a tick once the deeper stream would take under half that share or the
target is no longer cut (`_shed_red`); RED packets carry the depth set from the
next one on (`RedOpusEncoder.set_distance`). Driven with stand-ins.
"""
import asyncio
import os
import sys
from fractions import Fraction
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

from selkies.webrtc.codecs.base import EncodedPacket  # noqa: E402
from selkies.webrtc.codecs.red import RedOpusEncoder  # noqa: E402
from selkies.webrtc.rtcrtpsender import RTCRtpSender  # noqa: E402
from selkies import webrtc_mode  # noqa: E402
from selkies.webrtc_mode import AUDIO_SHARE, WebRTCService  # noqa: E402

res = H.Results("audio-share")


def blocks(red: bytes) -> int:
    """The blocks an RFC 2198 payload carries: 4-byte headers with the F bit, then the primary's 1-byte one."""
    n = i = 0
    while red[i] & 0x80:
        n, i = n + 1, i + 4
    return n + 1


encoder = RedOpusEncoder(distance=2)
pts = iter(range(0, 48000 * 10, 480))


def pack() -> int:
    packet = EncodedPacket(data=b"\x01" * 40, pts=next(pts), time_base=Fraction(1, 48000))
    return blocks(encoder.pack(packet)[0][0])


res.check("a RED stream of depth two carries two prior payloads once it has them",
          [pack() for _ in range(4)] == [1, 2, 3, 3])
encoder.set_distance(0)
res.check("at depth 0 the next packet carries the primary alone", pack() == 1)
encoder.set_distance(1)
res.check("a depth brought back refills from the packets sent after it", [pack() for _ in range(3)] == [1, 2, 2])
encoder.set_distance(None)
res.check("None is the configured depth again", encoder.distance == 2 and [pack() for _ in range(3)] == [2, 3, 3])

class Sender(SimpleNamespace):
    """An audio sender stand-in with the real RED depth accessors."""
    red_depth = RTCRtpSender.red_depth
    set_red_distance = RTCRtpSender.set_red_distance


sender = Sender(_RTCRtpSender__encoder=RedOpusEncoder(distance=2))
res.check("a RED sender reports its depth and the configured one", sender.red_depth == (2, 2))
res.check("one without RED reports none", Sender(_RTCRtpSender__encoder=object()).red_depth is None)

loop = SimpleNamespace(_audio_sent={})
bps = lambda s: WebRTCService._audio_bps(loop, "p", s)  # noqa: E731
octets = SimpleNamespace(sent_octets=(0, 0))
res.check("a first tick reads no audio rate", bps(octets) == 0.0)
at = loop._audio_sent["p"][1]
loop._audio_sent["p"] = (0, at - 1.0)
octets.sent_octets = (50_000, 100)
rate = bps(octets)
res.check("the next reads the octets sent over the time between them", 399_000 < rate <= 400_100, rate)
res.check("no audio sender reads none and forgets the peer", bps(None) == 0.0 and "p" not in loop._audio_sent)

shed = lambda audio, goodput, cut=True: WebRTCService._shed_red(loop, "p", sender, audio, goodput, cut)  # noqa
shed(400_000, 500_000)
res.check("a cut target with audio over AUDIO_SHARE of the delivery drops a payload", sender.red_depth == (1, 2))
shed(270_000, 500_000)
shed(140_000, 500_000)
res.check("a tick at a time, to depth 0 at the least", sender.red_depth == (0, 2))
shed(140_000, 500_000)
res.check("(it stays there)", sender.red_depth == (0, 2))
shed(140_000, 1_000_000)
res.check("one where the deeper stream would take more than half AUDIO_SHARE holds",
          sender.red_depth == (0, 2), (280_000, AUDIO_SHARE / 2 * 1_000_000))
shed(140_000, 1_200_000)
res.check("one where it would take less brings a payload back", sender.red_depth == (1, 2))
shed(270_000, 1_200_000, cut=False)
res.check("a target no longer cut brings the configured depth back", sender.red_depth == (2, 2)
          and sender._RTCRtpSender__encoder.distance == 2)
shed(400_000, 500_000, cut=False)
res.check("and nothing drops it while it is not cut", sender.red_depth == (2, 2))
plain = SimpleNamespace(red_depth=None, set_red_distance=lambda d: res.check("no RED: nothing to set", False))
WebRTCService._shed_red(loop, "q", plain, 400_000, 500_000, True)
zero = SimpleNamespace(red_depth=(0, 0), set_red_distance=lambda d: res.check("depth 0 configured: kept", False))
WebRTCService._shed_red(loop, "q", zero, 400_000, 500_000, True)



async def steered(audio_sender) -> list:
    """One congestion loop tick over a peer whose transport delivered 500 kbit/s, 400 of it the audio its
    `audio_sender` sent over the second before: what the steer was given."""
    given = []
    steer = SimpleNamespace(hold_until=0.0, restored=False, capacity_kbps=None,
                            target=lambda current, ceiling, floor, goodput, loss, now, queue_s, sent, **k:
                            given.append((goodput, sent)) or current)
    pipeline = SimpleNamespace(rc_mode=webrtc_mode.RateControlMode.CBR, video_bitrate=1000,
                               set_video_bitrate=AsyncMock())
    service = WebRTCService.__new__(WebRTCService)
    service.metrics, service.args = None, SimpleNamespace(congestion_control=True)
    service._congestion_steer = {"primary": steer}
    service._path_probes, service._padding, service._rate_holds, service._link_allowances = {}, {}, {}, {}
    service._audio_sent = {"peer": (0, 31.0)}
    service._display_setting = lambda did, key: 6000
    service.display_pipelines = {"primary": pipeline}
    service._ensure_pacer = Mock()
    service._display_owner_tabs, service._peer_tabs = {}, {}
    window = dict(goodput_bps=500_000, sent_bps=500_000, loss_fraction=0.0, queue_ms=0.0,
                  queue_rising_ms=None, queue_depth_ms=0.0)
    transport = SimpleNamespace(take_twcc_window=lambda: dict(window))
    service.rtc_app = SimpleNamespace(peer_connections={"peer": dict(
        peer_conn=SimpleNamespace(sctp=SimpleNamespace(transport=transport)), display_id="primary",
        video_sender=None, audio_sender=audio_sender)}, send_cc_rate=lambda did, kbps: None,
        set_video_budget=lambda did, bps: None)
    sleep = AsyncMock(side_effect=[None, asyncio.CancelledError()])
    with patch.object(webrtc_mode, "asyncio", SimpleNamespace(sleep=sleep)), \
            patch.object(webrtc_mode, "time", SimpleNamespace(monotonic=lambda: 32.0)), \
            patch.object(webrtc_mode, "settings", SimpleNamespace(video_bitrate=(100, 12000), webrtc_pacer=(True,))):
        try:
            await service._congestion_control_loop()
        except asyncio.CancelledError:
            pass
    return given


given = asyncio.run(steered(SimpleNamespace(sent_octets=(50_000, 100), red_depth=None)))
res.check("the steer is given what the path delivered and the stream sent less the audio's share of it",
          given == [(100_000, 100_000)], given)
given = asyncio.run(steered(None))
res.check("a peer without audio gives it all of it", given == [(500_000, 500_000)], given)

sys.exit(0 if res.summary() else 1)
