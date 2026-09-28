#!/usr/bin/env python3
"""What the WebRTC pipeline is asked while its capture is still starting reaches that capture.

A capture's first frames reach the video bridge before `start_capture` returns, so the bridge can
let a frame go and name it to the encoder, or ask for a keyframe, while the start is under way,
and a setter can change a rate or a tunable then. Dropped as meant for a capture that is not
running, the word leaves the encoder predicting from a frame the peer never gets, which with an
infinite GOP holds the page on its first frame; a dropped rate or tunable leaves the encoder
running what the pipeline no longer reports. A structural change made then restarts the capture
once the start is done. Driven with a stand-in capture whose start blocks until released; no
pixelflux, no peer.
"""
import asyncio
import os
import sys
import threading
from types import SimpleNamespace

for key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[key]

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

from selkies import webrtc_media_pipeline as wmp  # noqa: E402


class Capture:
    """A ScreenCapture stand-in whose first start blocks until the test releases it."""

    starts: list = []
    entered = threading.Event()
    release = threading.Event()

    def __init__(self) -> None:
        self.calls = []

    def set_cursor_callback(self, callback) -> None:
        pass

    def start_capture(self, callback, settings) -> None:
        Capture.starts.append(settings)
        if len(Capture.starts) == 1:
            Capture.entered.set()
            Capture.release.wait(10)

    def invalidate_reference(self, frame_id: int) -> None:
        self.calls.append(("invalidate_reference", frame_id))

    def request_idr_frame(self) -> None:
        self.calls.append(("request_idr_frame",))

    def update_video_bitrate(self, kbps: int) -> None:
        self.calls.append(("update_video_bitrate", kbps))

    def update_tunables(self, settings) -> None:
        self.calls.append(("update_tunables", settings.video_streaming_mode))


async def scenario(res: H.Results) -> None:
    wmp.ScreenCapture = Capture
    wmp.CaptureSettings = SimpleNamespace
    loop = asyncio.get_running_loop()
    pipeline = wmp.MediaPipelinePixel(async_event_loop=loop, encoder="h264enc", framerate=60,
                                      video_bitrate=8000, audio_enabled=False, width=640, height=480)
    start = asyncio.ensure_future(pipeline.start_media_pipeline(video=True, audio=False))
    await asyncio.to_thread(Capture.entered.wait, 10)
    capture = pipeline.capture_module
    res.check("the capture is starting", capture is not None and not start.done())

    pipeline.invalidate_reference(1)
    await pipeline.dynamic_idr_frame()
    await pipeline.set_video_bitrate(4000)
    turbo = not pipeline.video_streaming_mode
    await pipeline.set_video_streaming_mode(turbo)
    res.check("a lost frame the bridge names while the capture starts reaches it",
              ("invalidate_reference", 1) in capture.calls, capture.calls)
    res.check("so does a keyframe request", ("request_idr_frame",) in capture.calls, capture.calls)
    res.check("and a live rate and tunable",
              ("update_video_bitrate", 4000) in capture.calls and ("update_tunables", turbo) in capture.calls,
              capture.calls)

    encoder = asyncio.ensure_future(pipeline.set_encoder("vp8enc"))
    await asyncio.sleep(0.05)
    res.check("a structural change waits for the start rather than racing it",
              len(Capture.starts) == 1 and not encoder.done(), len(Capture.starts))
    Capture.release.set()
    await start
    await encoder
    res.check("and restarts the capture with it once the start is done",
              len(Capture.starts) == 2 and Capture.starts[1].codec == "vp8", [s.codec for s in Capture.starts])
    res.check("the capture reads running after its start", pipeline.is_screen_capturing())


def main() -> int:
    res = H.Results("capture-start-window")
    asyncio.run(scenario(res))
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
