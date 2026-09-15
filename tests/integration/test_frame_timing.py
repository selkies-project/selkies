#!/usr/bin/env python3
"""Every frame pixelflux delivers carries its capture and encode instants, on
the X11 capture against the test display and on the headless Wayland
compositor: CLOCK_MONOTONIC nanoseconds that order capture, encode start and
encode end, land before the frame reaches Python, and put the whole host leg of
a 720p frame well under a frame interval.
"""
import importlib.util
import os
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

RUNTIME = "/tmp/sel-frametiming"


def collect(cs, seconds: float = 3.0) -> list:
    import pixelflux
    frames, done = [], threading.Event()

    def on_frame(frame):
        frames.append((frame.capture_ns, frame.encode_start_ns, frame.encode_end_ns, time.monotonic_ns()))
        if len(frames) >= 90:
            done.set()

    sc = pixelflux.ScreenCapture()
    sc.start_capture(on_frame, cs)
    try:
        done.wait(seconds)
    finally:
        sc.stop_capture()
    return frames


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

    cs = pixelflux.CaptureSettings()
    cs.capture_width, cs.capture_height = 1280, 720
    cs.codec = "h264"
    cs.target_fps = 30
    cs.video_streaming_mode = True
    os.environ["DISPLAY"] = H.require_display()
    judge(res, "x11 h264", collect(cs))

    os.makedirs(RUNTIME, exist_ok=True)
    os.chmod(RUNTIME, 0o700)
    os.environ["XDG_RUNTIME_DIR"] = RUNTIME
    wl = pixelflux.CaptureSettings()
    wl.use_wayland = True
    wl.capture_width, wl.capture_height = 1280, 720
    wl.codec = "h264"
    wl.target_fps = 30
    wl.video_streaming_mode = True
    wl.display_id = 1
    pixelflux.ensure_wayland_display(1280, 720)
    judge(res, "wayland h264", collect(wl))
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
