#!/usr/bin/env python3
"""pixelflux's wire header names the picture the encoder produced.

The WebRTC video bridge reads a frame's keyframe flag off the per-stripe
header (the low nibble of byte 1: a keyframe is 0x01; every JPEG picture
stands alone) to keep the wire decodable across a drop, so the header has to
say so for every encoder and backend: the first H.264 picture of a capture is
an IDR, the pictures behind it are not, and a requested keyframe arrives as
one. Driven against pixelflux directly on a throwaway X server and on its own
Wayland compositor.

    python3 tests/integration/test_wire_header_picture_type.py
"""
import os
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

H264, JPEG = 0x04, 0x03
IDR = 0x01


class Headers:
    """Collects (tag, picture type) per delivered stripe."""

    def __init__(self) -> None:
        self.frames: list = []
        self.lock = threading.Lock()

    def __call__(self, frame) -> None:
        view = memoryview(frame)
        if len(view) >= 10:
            with self.lock:
                self.frames.append((view[0], view[1] & 0x0F))

    def snap(self) -> list:
        with self.lock:
            return list(self.frames)


def settings(pixelflux, encoder: str, cpu: bool, wayland: bool):
    cs = pixelflux.CaptureSettings()
    cs.capture_width, cs.capture_height = 640, 400
    cs.target_fps = 30.0
    cs.use_cpu = cpu
    cs.use_wayland = wayland
    cs.video_bitrate_kbps = 3000
    cs.video_crf = 25
    cs.video_cbr_mode = True
    cs.video_streaming_mode = True
    cs.use_paint_over_quality = True
    cs.omit_stripe_headers = False
    if hasattr(cs, "codec"):
        cs.codec = "jpeg" if encoder == "jpeg" else "h264"
    else:
        cs.output_mode = 0 if encoder == "jpeg" else 1
    cs.video_fullframe = encoder == "h264enc"
    return cs


def repaint(stop: threading.Event) -> None:
    """A striped capture sends only what damage covers, so keep the root changing."""
    colours = ("#3366cc", "#cc6633", "#33cc66")
    index = 0
    while not stop.wait(0.1):
        subprocess.run(["xsetroot", "-solid", colours[index % len(colours)]],
                       capture_output=True)
        index += 1


def capture(pixelflux, encoder: str, cpu: bool, wayland: bool,
            damage: bool = False) -> tuple:
    cap = pixelflux.ScreenCapture()
    sink = Headers()
    stop = threading.Event()
    if damage:
        threading.Thread(target=repaint, args=(stop,), daemon=True).start()
    try:
        cap.start_capture(sink, settings(pixelflux, encoder, cpu, wayland))
        time.sleep(2.0)
        before = sink.snap()
        cap.request_idr_frame()
        time.sleep(1.0)
        after = sink.snap()[len(before):]
        cap.stop_capture()
    finally:
        stop.set()
    time.sleep(0.3)
    return before, after


def check(res: H.Results, tag: str, before: list, after: list, jpeg: bool) -> None:
    res.check(f"{tag}: frames flow", len(before) >= 10 and len(after) >= 5,
              (len(before), len(after)))
    if not before:
        return
    if jpeg:
        res.check(f"{tag}: every stripe carries the JPEG tag",
                  all(t == JPEG for t, _ in before + after), sorted({t for t, _ in before}))
        return
    res.check(f"{tag}: every frame carries the H.264 tag",
              all(t == H264 for t, _ in before + after), sorted({t for t, _ in before}))
    res.check(f"{tag}: the first picture is an IDR", before[0][1] == IDR, before[0])
    others = [k for _, k in before[1:]]
    res.check(f"{tag}: the pictures behind it are not",
              others and not any(k == IDR for k in others[:len(others) // 2]),
              others[:12])
    res.check(f"{tag}: a requested keyframe arrives as an IDR",
              any(k == IDR for _, k in after), [k for _, k in after][:12])


def main() -> int:
    res = H.Results("wire-header")
    try:
        import pixelflux
    except Exception as e:
        res.skip("pixelflux importable", repr(e)[:120])
        res.summary()
        return H.SKIP_EXIT
    xproc, display = H.private_x_server(640, 400)
    os.environ["DISPLAY"] = display
    try:
        subprocess.run(["xsetroot", "-solid", "#3366cc"], capture_output=True)
        for encoder, cpu in (("h264enc", False), ("h264enc", True), ("jpeg", True)):
            tag = "x11 " + encoder + (" cpu" if cpu else "")
            try:
                before, after = capture(pixelflux, encoder, cpu, False,
                                        damage=encoder == "jpeg")
            except Exception as e:
                res.check(f"{tag}: capture", False, repr(e)[:140])
                continue
            check(res, tag, before, after, encoder == "jpeg")
        for cpu in (False, True):
            tag = "wayland h264enc" + (" cpu" if cpu else "")
            try:
                before, after = capture(pixelflux, "h264enc", cpu, True)
            except Exception as e:
                res.check(f"{tag}: capture", False, repr(e)[:140])
                continue
            check(res, tag, before, after, False)
    finally:
        H.stop_x_server(xproc, display)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
