#!/usr/bin/env python3
"""The WebRTC audio capture is restarted when its pcmflux worker dies.

pcmflux reports a clean start while the worker is still bringing the device up,
so a device that fails immediately after start_capture returned Ok surfaces only
later through last_error. MediaPipelinePixel.recover_audio_if_failed polls that
and cycles the audio capture through the normal stop/start path, rate-limited so
a permanently broken device does not restart on every tick; video is untouched.
Driven with a fake capture module — no PulseAudio, no pcmflux.
"""
import asyncio
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.media_pipeline import MediaPipelinePixel


class FakeModule:
    def __init__(self, state="running", last_error=None):
        self.state = state
        self.last_error = last_error


def make_pipeline() -> MediaPipelinePixel:
    p = MediaPipelinePixel(async_event_loop=asyncio.get_running_loop(),
                           encoder="h264enc", audio_enabled=True)
    p._running = True
    p._is_pcmflux_capturing = True
    p.starts = 0
    p.stops = 0

    async def fake_stop():
        p.stops += 1
        p._is_pcmflux_capturing = False
        p.pcmflux_module = None

    async def fake_start():
        p.starts += 1
        p._is_pcmflux_capturing = True
        p.pcmflux_module = FakeModule(state="running", last_error=None)

    p._stop_audio_pipeline = fake_stop
    p._start_audio_pipeline = fake_start
    return p


async def scenario(res: H.Results) -> None:
    # A healthy worker is left alone.
    p = make_pipeline()
    p.pcmflux_module = FakeModule(state="running", last_error=None)
    did = await p.recover_audio_if_failed()
    res.check("healthy worker: no restart", did is False and p.starts == 0, (did, p.starts))

    # A worker that reports a fresh start but has failed is restarted once.
    p = make_pipeline()
    p.pcmflux_module = FakeModule(state="starting", last_error="device vanished")
    did = await p.recover_audio_if_failed()
    res.check("failed worker: restarted through stop+start",
              did is True and p.stops == 1 and p.starts == 1, (did, p.stops, p.starts))
    res.check("failed worker: capture is live again after restart",
              p._is_pcmflux_capturing and getattr(p.pcmflux_module, "last_error", "x") is None, p.pcmflux_module)

    # A device that keeps failing does not restart on every tick (rate floor).
    p = make_pipeline()

    async def fail_start():
        p.starts += 1
        p._is_pcmflux_capturing = True
        p.pcmflux_module = FakeModule(state="failed", last_error="still broken")

    p._start_audio_pipeline = fail_start
    p.pcmflux_module = FakeModule(state="failed", last_error="still broken")
    first = await p.recover_audio_if_failed()
    second = await p.recover_audio_if_failed()
    res.check("permanent failure: one restart, then the floor blocks the next",
              first is True and second is False and p.starts == 1, (first, second, p.starts))
    # Past the floor, it tries again.
    p._audio_recover_last_attempt -= 10.0
    third = await p.recover_audio_if_failed()
    res.check("permanent failure: retried once the floor elapses",
              third is True and p.starts == 2, (third, p.starts))

    # Audio disabled or capture already stopped: never touched.
    p = make_pipeline()
    p.audio_enabled = False
    p.pcmflux_module = FakeModule(last_error="ignored")
    res.check("audio disabled: no restart",
              await p.recover_audio_if_failed() is False and p.starts == 0, p.starts)

    p = make_pipeline()
    p._is_pcmflux_capturing = False
    p.pcmflux_module = FakeModule(last_error="ignored")
    res.check("not capturing: no restart",
              await p.recover_audio_if_failed() is False and p.starts == 0, p.starts)


def main() -> bool:
    res = H.Results("webrtc-audio-recovery")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
