#!/usr/bin/env python3
"""A capture that streams another codec than the encoder asked for is a
demotion: the pipeline reads the capture's active codec once frames flow,
takes the encoder that codec belongs to, and tells its transport; a capture
streaming the requested codec, or one that has not decided yet, changes
nothing.
"""
import asyncio
import os
import sys
import types

for key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[key]

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))

from selkies.media_pipeline import MediaPipelinePixel  # noqa: E402
from selkies.settings import encoder_for_codec  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [encoder-demotion] {label}  {detail}", flush=True)


check("a codec's full-frame encoder", [encoder_for_codec(c) for c in ("h264", "h265", "vp8", "vp9", "av1", "jpeg")]
      == ["h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc", "jpeg"])
check("an unknown codec lands on H.264", encoder_for_codec("mpeg2") == "h264enc")


async def settle(encoder: str, active):
    loop = asyncio.get_running_loop()
    pipeline = MediaPipelinePixel(async_event_loop=loop, encoder=encoder, framerate=30, video_bitrate=8000,
                                  audio_enabled=False, width=640, height=480)
    told = []
    pipeline.on_encoder_demoted = told.append
    pipeline.capture_module = types.SimpleNamespace(active_codec=lambda: active)
    pipeline._is_screen_capturing = True
    await pipeline._settle_active_codec(5)
    return pipeline.encoder, told


encoder, told = asyncio.run(settle("av1enc", "h264"))
check("a capture streaming H.264 for av1enc demotes to h264enc", encoder == "h264enc" and told == ["h264enc"], (encoder, told))
encoder, told = asyncio.run(settle("av1enc", "av1"))
check("a capture streaming the requested codec changes nothing", encoder == "av1enc" and told == [], (encoder, told))
encoder, told = asyncio.run(settle("vp9enc", None))
check("an undecided capture changes nothing", encoder == "vp9enc" and told == [], (encoder, told))

print(f"{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
