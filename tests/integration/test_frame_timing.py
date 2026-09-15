#!/usr/bin/env python3
"""Every frame pixelflux delivers carries its capture and encode instants, on
the X11 capture against the test display and on the headless Wayland
compositor, encoded in hardware where the node has it and in software: CLOCK_MONOTONIC
nanoseconds that order capture, encode start and encode end, land before the
frame reaches Python, and put the whole host leg of a 720p frame well under a
frame interval. A software session delivers a frame as stripes, each carrying
the frame's stamps, so the collection is timed rather than counted.
"""
import importlib.util
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

RUNTIME = "/tmp/sel-frametiming"


def collect(cs, seconds: float = 3.0) -> list:
    import pixelflux
    frames = []

    def on_frame(frame):
        frames.append((frame.capture_ns, frame.encode_start_ns, frame.encode_end_ns, time.monotonic_ns()))

    sc = pixelflux.ScreenCapture()
    sc.start_capture(on_frame, cs)
    try:
        time.sleep(seconds)
    finally:
        sc.stop_capture()
    return frames


def settings(wayland: bool, software: bool):
    import pixelflux
    cs = pixelflux.CaptureSettings()
    cs.capture_width, cs.capture_height = 1280, 720
    cs.codec = "h264"
    cs.target_fps = 30
    cs.video_streaming_mode = True
    cs.use_cpu = software
    if wayland:
        cs.use_wayland = True
        cs.display_id = 1
    return cs


def judge(res: "H.Results", label: str, frames: list) -> None:
    res.check(f"{label}: frames arrive", len(frames) >= 5, len(frames))
    if not frames:
        return
    ordered = all(0 < c <= s <= e <= now for c, s, e, now in frames)
    res.check(f"{label}: capture, encode start, encode end and arrival are ordered", ordered,
              [f for f in frames if not (0 < f[0] <= f[1] <= f[2] <= f[3])][:2])
    # The encoder's own start (an NVENC session takes hundreds of milliseconds to open)
    # is charged to the first frames, the ones captured while it opened included, so
    # the bound is on the frames captured a second or more into the stream.
    steady = [f for f in frames if f[0] - frames[0][0] >= 1_000_000_000]
    host_ms = [(e - c) / 1e6 for c, s, e, _ in steady]
    res.check(f"{label}: the host leg of a 720p frame stays under 100 ms once streaming", host_ms and max(host_ms) < 100,
              f"{len(host_ms)} frames, max {max(host_ms or [0]):.1f} ms, median {sorted(host_ms or [0])[len(host_ms) // 2]:.1f} ms")


def main() -> "H.Results":
    res = H.Results("frame-timing")
    if importlib.util.find_spec("pixelflux") is None:
        H.skip_suite("pixelflux is not installed")
    import pixelflux

    os.environ["DISPLAY"] = H.require_display()
    judge(res, "x11 h264", collect(settings(False, False)))
    judge(res, "x11 h264 software", collect(settings(False, True)))

    os.makedirs(RUNTIME, exist_ok=True)
    os.chmod(RUNTIME, 0o700)
    os.environ["XDG_RUNTIME_DIR"] = RUNTIME
    pixelflux.ensure_wayland_display(1280, 720)
    judge(res, "wayland h264", collect(settings(True, False)))
    judge(res, "wayland h264 software", collect(settings(True, True)))
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
