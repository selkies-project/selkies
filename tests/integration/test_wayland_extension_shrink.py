#!/usr/bin/env python3
"""On WebRTC, a primary that shrinks beside a right-hand secondary keeps the
second screen.

Each display is a screen of the session compositor's own. When the primary
shrinks, the secondary moves into the room it frees, and the compositor refuses
an output rectangle that overlaps a live one: the layout pass must shrink the
primary's screen before it recreates the secondary at its new offset, or the
second screen is dropped on every primary shrink. Drives the real layout method
(WebRTCService._apply_wayland_extension) against the in-process pixelflux
compositor -- no browser, no signaling.
"""
import asyncio
import importlib.util
import os
import sys
import time
import types

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

RUNTIME = "/tmp/sel-wlext"


def build_service(sc):
    from selkies.webrtc_mode import WebRTCService
    svc = WebRTCService.__new__(WebRTCService)
    svc.media_pipeline = types.SimpleNamespace(capture_module=sc)
    svc._wayland_ctl_module = None
    svc.input_handler = None
    svc._display_dpis = {}
    svc._last_applied_dpi = 96
    return svc


def drive(res: "H.Results") -> None:
    import pixelflux
    sys.path.insert(0, os.path.join(H.REPO, "src"))
    from selkies.display_utils import wayland_output_id, WAYLAND_SCREEN_OUTPUT_ID

    cs = pixelflux.CaptureSettings()
    cs.use_wayland = True
    cs.capture_width, cs.capture_height = 1920, 1080
    cs.auto_adjust_screen_capture_size = True
    cs.codec = "jpeg"
    cs.target_fps = 10
    cs.scale = 1.0
    cs.jpeg_quality = 40
    cs.omit_stripe_headers = False
    pixelflux.ensure_wayland_display(1920, 1080)
    sc = pixelflux.ScreenCapture()
    sc.start_capture(lambda f: None, cs)
    try:
        deadline = time.time() + 15
        while time.time() < deadline and not any(
                o[0] == WAYLAND_SCREEN_OUTPUT_ID for o in sc.list_outputs()):
            time.sleep(0.2)
        did = "display2"
        oid = wayland_output_id(did)
        res.check("primary screen up at 1920x1080",
                  next((o for o in sc.list_outputs() if o[0] == WAYLAND_SCREEN_OUTPUT_ID), (0,)*5)[3] == 1920, "")
        res.check("secondary created at the primary's right edge",
                  bool(sc.create_output(oid, 1280, 720, 1920, 0, 1.0)), "")

        svc = build_service(sc)
        layouts = {
            "primary": {"x": 0, "y": 0, "w": 1280, "h": 720},
            did: {"x": 1280, "y": 0, "w": 1280, "h": 720},
        }
        ok = asyncio.run(svc._apply_wayland_extension(did, layouts))
        outs = {o[0]: o for o in sc.list_outputs()}
        res.check("the secondary was not dropped on the shrink", ok, outs)
        sec = outs.get(oid)
        res.check("the secondary sits at the shrunken primary's edge (+1280)",
                  sec is not None and sec[1] == 1280, sec)
        res.check("the primary screen shrank to 1280x720",
                  WAYLAND_SCREEN_OUTPUT_ID in outs
                  and outs[WAYLAND_SCREEN_OUTPUT_ID][3] == 1280
                  and outs[WAYLAND_SCREEN_OUTPUT_ID][4] == 720,
                  outs.get(WAYLAND_SCREEN_OUTPUT_ID))
    finally:
        sc.stop_capture()


def main() -> "H.Results":
    res = H.Results("wl-extension-shrink")
    if importlib.util.find_spec("pixelflux") is None:
        H.skip_suite("pixelflux is not installed")
    os.makedirs(RUNTIME, exist_ok=True)
    os.chmod(RUNTIME, 0o700)
    os.environ["XDG_RUNTIME_DIR"] = RUNTIME
    drive(res)
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
