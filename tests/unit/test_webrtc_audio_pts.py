#!/usr/bin/env python3
"""The WebRTC audio clock across pcmflux capture restarts.

pcmflux re-zeros its sample clock on every capture start, and the RTP sender
lives on, so MediaPipelinePixel maps each capture's pts onto one continuous
48 kHz clock (`_audio_rtp_pts`). Within a capture the frames keep pcmflux's
spacing, gated silence included. A restart lands its first frame as far past
the last one as the wall clock moved (an audio pause and resume, or a capture
that opens on gated silence), never less than one frame past it (a restart
inside a frame), and never behind it. Driven with frame pts and delivery times
only: no PulseAudio, no pcmflux.
"""
import asyncio
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.webrtc_media_pipeline import MediaPipelinePixel

FRAME = 480  # 10 ms at 48 kHz


def make_pipeline() -> MediaPipelinePixel:
    p = MediaPipelinePixel(async_event_loop=asyncio.get_running_loop(),
                           encoder="h264enc", audio_enabled=True)
    p._audio_frame_samples = FRAME
    return p


def capture(p: MediaPipelinePixel, start: float, frames, first_raw: int = 0) -> list:
    """One capture session: bump the epoch as a start does, then deliver
    `frames` frames 10 ms apart from monotonic time `start`, the first
    carrying pcmflux pts `first_raw`. Returns the mapped pts."""
    p._audio_capture_epoch += 1
    return [p._audio_rtp_pts(first_raw + i * FRAME, start + first_raw / 48000 + i * 0.01)
            for i in range(frames)]


async def scenario(res: H.Results) -> None:
    p = make_pipeline()
    a = capture(p, 100.0, 50)
    res.check("first capture: pcmflux's own clock", a == [i * FRAME for i in range(50)], a[:3])

    # Gated silence inside a capture: pcmflux's pts already skips it.
    p._audio_rtp_pts(50 * FRAME + 48000, 100.0 + 0.5 + 1.0)
    res.check("gated silence inside a capture keeps pcmflux's spacing",
              p._audio_last_pts == a[-1] + FRAME + 48000, p._audio_last_pts - a[-1])

    # Paused for a second, then restarted: the clock moved on by that second.
    last, wall = p._audio_last_pts, p._audio_last_wall
    b = capture(p, wall + 1.0, 20)
    res.check("a restart after a one-second pause lands a second later",
              b[0] - last == 48000, b[0] - last)
    res.check("frames after the restart keep their spacing",
              all(y - x == FRAME for x, y in zip(b, b[1:])), b[:3])

    # A capture that opens on gated silence delivers its first frame late,
    # carrying a later pcmflux pts: the wall clock still decides.
    last, wall = p._audio_last_pts, p._audio_last_wall
    c = capture(p, wall + 0.2, 5, first_raw=24000)
    res.check("a restart opening on half a second of silence lands 0.7 s later",
              c[0] - last == round(0.7 * 48000), c[0] - last)

    # A restart inside one frame's time: one frame on, never on top of the last.
    last, wall = p._audio_last_pts, p._audio_last_wall
    d = capture(p, wall + 0.001, 3)
    res.check("a restart inside a frame lands one frame on", d[0] - last == FRAME, d[0] - last)

    # A delivery clock that reads earlier than the last frame's (a delayed
    # callback) never rewinds the clock.
    last, wall = p._audio_last_pts, p._audio_last_wall
    e = capture(p, wall - 0.05, 3)
    res.check("the clock never runs backward", e[0] - last == FRAME, e[0] - last)

    series = a + b + c + d + e
    res.check("pts rise strictly across all five captures",
              all(y > x for x, y in zip(series, series[1:])), len(series))


def main() -> bool:
    res = H.Results("webrtc-audio-pts")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
