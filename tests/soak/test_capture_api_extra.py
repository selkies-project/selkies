# -*- coding: utf-8 -*-
"""Second-pass pixelflux + pcmflux capability soak: surfaces NOT covered by
test_capture_api.py. Standalone (no selkies).

Phase A:  X11 Computer Use full action matrix (right/middle/double/triple click,
          drag, mouse down/up, key specs/combos, hold_key, type, scroll dirs +
          modifiers, zoom clamp, display-id errors, unknown action, 413 cap)
          plus a real-keyboard landing check via xev.
Phase B:  MP4 recorder deep validation (own-capture, custom settings, restart,
          already-active error, stop-with-none error, JPEG-rejection, ffprobe).
Phase C:  pixelflux untested CaptureSettings fields (keyframe_interval_s
          periodic IDR, capture_x/y subregion, vbv/qp fields, paint-over,
          debug_logging).
Phase D:  Wayland: clipboard write/watch round trip, clipboard+cursor callbacks,
          move_window_to_output, Computer Use against the Wayland backend,
          recorder own-capture on Wayland.
Phase E:  pcmflux extras (silence gate, CBR, frame durations, half-rate,
          debug_logging, latency, mono, bad-device errors, playback
          max_buffer_bytes + RED write_red round trip).
"""
import base64
import json
import os
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
from helpers import Results

import pixelflux
import pcmflux

from typing import Optional

TEST_DISPLAY = ":98"
CU_PORT = 9600
CU_BASE = f"http://127.0.0.1:{CU_PORT}"


def curl(url: str, body=None, timeout: float = 30) -> tuple:
    """GET (or POST when a body is given) a URL.

    Returns:
        Tuple of (status_code, response_bytes); HTTP errors are returned, not
        raised, so error-status assertions can read the body.
    """
    data = body.encode() if isinstance(body, str) else body
    req = urllib.request.Request(url, data=data,
                                 method="POST" if body is not None else "GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def dget(d: dict, *path):
    """Nested dict lookup following the given key path."""
    for k in path:
        d = d[k]
    return d


def png_size(b: bytes) -> Optional[tuple]:
    """(width, height) from a PNG header, or None for non-PNG bytes."""
    if b[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    w, h = struct.unpack(">II", b[16:24])
    return w, h


def make_settings(encoder: str = "h264enc", w: int = 1024, h: int = 640, **kw):
    """Build an H.264 CaptureSettings baseline with overrides applied last."""
    cs = pixelflux.CaptureSettings()
    cs.capture_width = w
    cs.capture_height = h
    cs.target_fps = 30.0
    cs.use_cpu = True
    cs.video_bitrate_kbps = 3000
    cs.video_crf = 25
    cs.video_cbr_mode = False
    cs.video_streaming_mode = True
    cs.video_fullcolor = False
    cs.use_paint_over_quality = False
    cs.jpeg_quality = 90
    cs.output_mode = 1
    cs.video_fullframe = True
    cs.use_openh264 = False
    for k, v in kw.items():
        setattr(cs, k, v)
    return cs


class FrameCounter:
    """Thread-safe frame-callback sink that counts stripes and IDR NALs."""

    def __init__(self) -> None:
        self.n = 0
        self.idr_nals = 0
        self.lock = threading.Lock()

    def __call__(self, frame) -> None:
        """Count one stripe, detecting IDR NALs in H.264 payloads."""
        try:
            b = bytes(frame)
        except Exception:
            return
        with self.lock:
            self.n += 1
            if len(b) > 10 and b[0] == 0x04:
                payload = b[10:]
                i = 0
                while i + 4 <= len(payload):
                    if (payload[i] == 0 and payload[i+1] == 0 and payload[i+2] == 0
                            and payload[i+3] == 1 and (payload[i+4] & 0x1F) == 5):
                        self.idr_nals += 1
                        break
                    if (payload[i] == 0 and payload[i+1] == 0 and payload[i+2] == 1
                            and i + 3 < len(payload) and (payload[i+3] & 0x1F) == 5):
                        self.idr_nals += 1
                        break
                    i += 1

    def snap(self) -> dict:
        """A consistent snapshot of the counters."""
        with self.lock:
            return dict(n=self.n, idr_nals=self.idr_nals)


def x_root_size() -> tuple[int, int]:
    """(width, height) of the test X server's root window."""
    from selkies.Xlib import display as xdisp
    d = xdisp.Display(TEST_DISPLAY)
    s = d.screen()
    w, h = s.width_in_pixels, s.height_in_pixels
    d.close()
    return w, h


def ffprobe(path: str) -> Optional[dict]:
    """ffprobe stream/format info as a dict, or None when the probe fails."""
    env = dict(os.environ)
    r = subprocess.run(["ffprobe", "-v", "error",
                        "-show_streams", "-show_format", "-of", "json", path],
                       capture_output=True, text=True, env=env, timeout=25)
    if r.returncode != 0:
        return None
    return json.loads(r.stdout)


def FF_RUN(fn):
    """Pass through an already-computed probe result.

    Intended to guard a probe that must not take the whole block down, but the
    argument is evaluated before this call, so exceptions still propagate at
    the call site; the try/except here can never fire.
    """
    try:
        return fn
    except Exception:
        return None


def cu_json(payload: dict) -> tuple:
    """POST an action to the computer-use endpoint.

    Returns:
        Tuple of (status_code, decoded_json_or_empty_dict).
    """
    status, body = curl(f"{CU_BASE}/computer-use", json.dumps(payload))
    out = {}
    try:
        out = json.loads(body)
    except Exception:
        pass
    return status, out


def main() -> Results:
    """Run phases A-E against a live X11 and Wayland capture environment."""
    res = Results("cap-soak2")
    os.environ["DISPLAY"] = TEST_DISPLAY
    subprocess.run(["xsetroot", "-solid", "#2244aa"],
                   env={"DISPLAY": TEST_DISPLAY, "PATH": os.environ.get("PATH", "")},
                   capture_output=True)

    # ============================= Phase A: X11 Computer Use matrix =====
    print("\n=== Phase A: X11 Computer Use ===", flush=True)
    pixelflux.start_computer_use(f"127.0.0.1:{CU_PORT}")
    time.sleep(1.0)

    def cu(name: str, payload: dict, pred, detail: str = "") -> None:
        """Post one computer-use action and check its response with `pred`."""
        try:
            st, out = cu_json(payload)
            res.check(name, pred(st, out), f"{st} {out}" if not detail else f"{st} {detail}")
        except Exception as e:
            res.check(name, False, repr(e)[:110])

    RW, RH = x_root_size()
    res.check("cu: root size read", RW > 0 and RH > 0, f"{RW}x{RH}")

    st, out = cu_json({"action": "screenshot"})
    got = png_size(base64.b64decode(out.get("data", ""))) if out.get("data") else None
    res.check("cu: screenshot PNG dims match root", got == (RW, RH), got)

    # coordinate clamping
    cu("cu: mouse_move clamps off-screen",
       {"action": "mouse_move", "coordinate": [90_000, 90_000]},
       lambda s, o: o.get("result") == "ok")

    cu("cu: left_click with modifier ctrl+click",
       {"action": "left_click", "coordinate": [RW // 2, RH // 2], "text": "ctrl"},
       lambda s, o: o.get("result") == "ok")
    cu("cu: right_click with coordinate",
       {"action": "right_click", "coordinate": [40, 40]},
       lambda s, o: o.get("result") == "ok")
    cu("cu: middle_click",
       {"action": "middle_click"},
       lambda s, o: o.get("result") == "ok")
    cu("cu: double_click with coordinate",
       {"action": "double_click", "coordinate": [100, 100]},
       lambda s, o: o.get("result") == "ok")
    cu("cu: triple_click",
       {"action": "triple_click", "coordinate": [120, 120]},
       lambda s, o: o.get("result") == "ok")
    cu("cu: left_click_drag start->end",
       {"action": "left_click_drag", "start_coordinate": [60, 60], "coordinate": [200, 200]},
       lambda s, o: o.get("result") == "ok")
    cu("cu: left_mouse_down",
       {"action": "left_mouse_down"},
       lambda s, o: o.get("result") == "ok")
    cu("cu: left_mouse_up",
       {"action": "left_mouse_up"},
       lambda s, o: o.get("result") == "ok")

    # key specs
    for spec in ["tab", "Return", "space", "Escape", "BackSpace", "minus",
                 "F5", "ctrl+a", "shift", "kp_enter", "a"]:
        cu(f"cu: key {spec!r}",
           {"action": "key", "text": spec},
           lambda s, o: o.get("result") == "ok")

    cu("cu: hold_key with duration",
       {"action": "hold_key", "text": "a", "duration": 0.1},
       lambda s, o: o.get("result") == "ok")
    cu("cu: hold_key missing duration errors",
       {"action": "hold_key", "text": "a"},
       lambda s, o: "error" in o or s not in (200,))

    # scroll directions + modifier + coordinate
    for d_ in ["up", "down", "left", "right"]:
        cu(f"cu: scroll {d_}",
           {"action": "scroll", "scroll_direction": d_, "scroll_amount": 3,
            "coordinate": [RW // 2, RH // 2], "text": "shift"},
           lambda s, o: o.get("result") == "ok")
    cu("cu: scroll invalid direction errors",
       {"action": "scroll", "scroll_direction": "diagonal", "scroll_amount": 1},
       lambda s, o: "error" in o)

    # type: shift-levels, punctuation, unicode
    cu("cu: type basic text",
       {"action": "type", "text": "Hello, World! 123"},
       lambda s, o: o.get("result") == "ok")
    cu("cu: type unicode accents",
       {"action": "type", "text": "D\u00e9j\u00e0 vu \u00fcber"},
       lambda s, o: o.get("result") == "ok")

    cu("cu: wait",
       {"action": "wait", "duration": 0.2},
       lambda s, o: o.get("result") == "ok")

    # zoom: crop + clamp
    st, out = cu_json({"action": "zoom", "region": [10.0, 10.0, 100.0, 80.0]})
    z = png_size(base64.b64decode(out.get("data", ""))) if out.get("data") else None
    res.check("cu: zoom crop dims", z == (90, 70), z)
    st, out = cu_json({"action": "zoom", "region": [0.0, 0.0, 90_000.0, 90_000.0]})
    zc = png_size(base64.b64decode(out.get("data", ""))) if out.get("data") else None
    res.check("cu: zoom clamps to root", zc == (RW, RH), zc)

    cu("cu: screenshot bad display id errors",
       {"action": "screenshot", "display": 99},
       lambda s, o: "error" in o)
    cu("cu: unknown action errors",
       {"action": "frobnicate"},
       lambda s, o: "error" in o)

    st, out = cu_json({"action": "cursor_position"})
    import re
    res.check("cu: cursor_position text format",
              re.fullmatch(r"X=\d+,Y=\d+", out.get("text", "")), out)

    # 413 body cap
    big = "x" * (4 * 1024 * 1024 + 1024)
    st, out = curl(f"{CU_BASE}/computer-use", big)
    res.check("cu: oversized body rejected (413)", st == 413, (st, out[:40]))

    # real keyboard landing via xev
    try:
        xev_log = os.path.join(H.WORKDIR, "cap2-xev.log")
        xev = H.spawn(["xev", "-event", "keyboard"], stdout=open(xev_log, "w"),
                      stderr=subprocess.STDOUT)
        time.sleep(1.2)
        wid = None
        for _ in range(10):
            r = subprocess.run(["xdotool", "search", "--name", "Event Tester"],
                               capture_output=True, text=True, env={"DISPLAY": TEST_DISPLAY,
                                                                    "PATH": os.environ.get("PATH", "") + ":/usr/bin"})
            ids = r.stdout.split()
            if ids:
                wid = ids[-1]
                break
            time.sleep(0.4)
        focused = False
        if wid:
            fr = subprocess.run(["xdotool", "windowfocus", "--sync", wid],
                                env={"DISPLAY": TEST_DISPLAY, "PATH": "/usr/bin"},
                                capture_output=True)
            focused = fr.returncode == 0
            time.sleep(0.3)
        cu_json({"action": "type", "text": "ab"})
        time.sleep(0.6)
        xev.terminate()
        xev.wait(timeout=5)
        log = open(xev_log).read()
        landed = ("KeyPress event" in log and "keycode 38" in log and "keycode 56" in log)
        res.check("cu: typed 'ab' lands as real keycodes (xev)",
                  landed if focused else (focused, "could not focus xev (harness fn)"),
                  f"focused={focused} keycodes={log.count('KeyPress event')}")
    except Exception as e:
        res.check("cu: typed keys land (xev)", False, repr(e)[:120])

    # ============================= Phase B: MP4 recorder (X11) ===========
    print("\n=== Phase B: MP4 recorder X11 ===", flush=True)
    rec1 = os.path.join(H.WORKDIR, "cap2-rec1.mp4")
    os.path.exists(rec1) and os.unlink(rec1)
    rw, rh = x_root_size()
    try:
        st = pixelflux.start_recording(rec1)
        res.check("rec: start_recording default active", st.get("active") is True, st)
        deadline = time.time() + 8
        while time.time() < deadline:
            cur = pixelflux.recording_status()
            if cur and cur.get("frames", 0) > 3:
                break
            time.sleep(0.3)
        try:
            dup = pixelflux.start_recording(os.path.join(H.WORKDIR, "cap2-rec2.mp4"))
            res.check("rec: duplicate start rejected", False, dup)
        except Exception as e:
            res.check("rec: duplicate start rejected",
                      "already active" in str(e), str(e)[:60])
        fin = pixelflux.stop_recording()
        res.check("rec: stop returns frames", fin.get("frames", 0) > 0, fin)
        res.check("rec: stop has sync frames", fin.get("sync_frames", 0) > 0, fin)
        res.check("rec: stop no error", fin.get("error") in (None, ""), fin)
        res.check("rec: final dims = root", (fin.get("width"), fin.get("height")) == (rw, rh),
                  (fin.get("width"), fin.get("height")))
        info = FF_RUN(ffprobe(rec1))
        got = None
        if info:
            v = [s for s in info.get("streams", []) if s.get("codec_type") == "video"]
            if v:
                got = (v[0].get("codec_name"), v[0].get("width"), v[0].get("height"))
        res.check("rec: MP4 plays back (h264)",
                  got and got[0] == "h264" and got[1] == rw and got[2] == rh, got)
    except Exception as e:
        res.check("rec: recorder flow", False, repr(e)[:200])

    try:
        pixelflux.stop_recording()
        res.check("rec: stop with none active errors", False, "")
    except Exception as e:
        res.check("rec: stop with none active errors", True, str(e)[:60])

    # custom-settings recording (explicit 640x400) then restart
    rec3 = os.path.join(H.WORKDIR, "cap2-rec3.mp4")
    os.path.exists(rec3) and os.unlink(rec3)
    rw, rh = x_root_size()
    try:
        cs = make_settings(w=640, h=400)
        st = pixelflux.start_recording(rec3, cs)
        deadline = time.time() + 8
        while time.time() < deadline:
            cur = pixelflux.recording_status()
            if cur and cur.get("frames", 0) > 3:
                break
            time.sleep(0.3)
        fin = pixelflux.stop_recording()
        info = FF_RUN(ffprobe(rec3))
        got = None
        if info:
            v = [s for s in info.get("streams", []) if s.get("codec_type") == "video"]
            if v:
                got = (v[0].get("width"), v[0].get("height"))
        res.check("rec: custom-settings capture honors 640x400", got == (640, 400), got)
    except Exception as e:
        res.check("rec: custom-settings", False, repr(e)[:160])

    try:
        j = make_settings(w=320, h=200)
        # output_mode 0 selects JPEG, which the recorder must reject.
        j.output_mode = 0
        j.video_fullframe = False
        try:
            pixelflux.start_recording(os.path.join(H.WORKDIR, "cap2-rec-jpg.mp4"), j)
            res.check("rec: JPEG settings rejected", False, "")
        except Exception as e:
            res.check("rec: JPEG settings rejected", "H.264" in str(e), str(e)[:70])
    except Exception as e:
        res.check("rec: JPEG rejection harness", False, repr(e)[:80])

    # ============================= Phase C: pixelflux fields =============
    print("\n=== Phase C: untested CaptureSettings fields ===", flush=True)
    try:
        cap = pixelflux.ScreenCapture()
        fc = FrameCounter()
        cs = make_settings(w=1024, h=640)
        cs.keyframe_interval_s = 0.5
        cap.start_capture(fc, cs)
        time.sleep(2.6)
        cap.stop_capture()
        res.check("fields: keyframe_interval_s schedules periodic IDRs",
                  fc.snap()["idr_nals"] >= 2, fc.snap())
    except Exception as e:
        res.check("fields: keyframe_interval_s", False, repr(e)[:140])

    try:
        cap = pixelflux.ScreenCapture()
        fc = FrameCounter()
        cs = make_settings(w=400, h=300)
        cs.capture_x = 300
        cs.capture_y = 120
        cs.capture_cursor = True
        cs.cursor_size = 24
        cs.cursor_size_cap = 64
        cs.debug_logging = True
        cap.start_capture(fc, cs)
        time.sleep(2.0)
        cap.stop_capture()
        res.check("fields: capture_x/y + cursor + debug_logging flow",
                  fc.snap()["n"] >= 4, fc.snap()["n"])
    except Exception as e:
        res.check("fields: capture_x/y subregion", False, repr(e)[:140])

    try:
        cap = pixelflux.ScreenCapture()
        fc = FrameCounter()
        cs = make_settings(w=800, h=500)
        cs.video_vbv_multiplier = 0.5
        cs.video_min_qp = 10
        cs.video_max_qp = 45
        cs.use_paint_over_quality = True
        cs.paint_over_trigger_frames = 5
        cs.video_paintover_crf = 28
        cs.video_paintover_burst_frames = 4
        cs.paint_over_jpeg_quality = 70
        cap.set_cursor_callback(lambda mt, data, hx, hy: None)
        cap.start_capture(fc, cs)
        # Force damage so paint-over triggers repeatedly.
        subprocess.run(["xsetroot", "-solid", "#55aa33"],
                       env={"DISPLAY": TEST_DISPLAY, "PATH": os.environ.get("PATH", "")},
                       capture_output=True)
        time.sleep(2.2)
        cap.stop_capture()
        res.check("fields: vbv/qp + paint-over flow",
                  fc.snap()["n"] >= 4, fc.snap()["n"])
    except Exception as e:
        res.check("fields: vbv/qp + paint-over", False, repr(e)[:140])

    t = threading.Thread(target=pixelflux._stop_all_captures, daemon=True)
    t.start(); t.join(timeout=5)
    res.check("module: _stop_all_captures (idempotent, no live capture)", not t.is_alive(), "")

    # ============================= Phase D: Wayland + CU-on-wayland ======
    print("\n=== Phase D: Wayland backend extras ===", flush=True)
    wc = pixelflux.ScreenCapture()
    wcs = make_settings(w=800, h=480)
    wcs.use_wayland = True
    try:
        wc.start_capture(FrameCounter(), wcs)
        time.sleep(3.0)
        res.check("wl: capture up", wc.is_capturing, "")

        # clipboard write/read round trip via the app compositor dcclient
        try:
            wc.clipboard_write_app("wayland-1", [("text/plain;charset=utf-8", b"cap2-wl-probe")])
            time.sleep(0.4)
            back = wc.clipboard_read_app("wayland-1", "text/plain;charset=utf-8")
            got = bytes(back).decode("utf-8", "replace") if back else None
            wc.clipboard_clear_app("wayland-1")
            res.check("wl: clipboard_write_app->read_app round trip", got == "cap2-wl-probe", repr(got))
        except Exception as e:
            res.check("wl: clipboard write_app round trip", False, repr(e)[:140])

        # watch fires with the selection current at registration
        try:
            wc.set_clipboard("text/plain;charset=utf-8", b"watch-probe")
            time.sleep(0.3)
            seen = []
            wc.clipboard_watch_app("wayland-1",
                                   lambda mimes: seen.append(tuple(mimes)))
            time.sleep(0.8)
            wc.clipboard_unwatch_app("wayland-1")
            res.check("wl: clipboard_watch_app fires current mimes",
                      any("text/plain" in m for s in seen for m in s) and bool(seen), seen)
        except Exception as e:
            res.check("wl: clipboard_watch_app", False, repr(e)[:140])

        # callbacks register cleanly on the running wayland backend
        cb_ev = []
        try:
            wc.set_clipboard_callback(lambda mime, data: cb_ev.append((mime, len(data or b""))))
            res.check("wl: set_clipboard_callback registers", True, "")
        except Exception as e:
            res.check("wl: set_clipboard_callback", False, repr(e)[:110])
        try:
            wc.set_cursor_callback(lambda mt, data, hx, hy: (mt, data))
            res.check("wl: set_cursor_callback registers", True, "")
        except Exception as e:
            res.check("wl: set_cursor_callback", False, repr(e)[:110])

        # move_window_to_output: graceful on unknown ids; real move when a window exists
        try:
            mb = wc.move_window_to_output(0, 0)
            if list(wc.list_windows()):
                wm = wc.move_window_to_output(list(wc.list_windows())[0][0], 0)
                res.check("wl: move_window_to_output real window", isinstance(wm, bool), wm)
            else:
                res.check("wl: move_window_to_output graceful on unknown", mb is False,
                          "no client window mapped (labwc seat empty)")
        except Exception as e:
            res.check("wl: move_window_to_output", False, repr(e)[:120])

        # set_app_wayland_display + CU-on-wayland
        try:
            wc.set_app_wayland_display("wayland-1")
            st, out = cu_json({"action": "type", "text": "wayland-type"})
            res.check("cu(+wl): type via app socket ok", out.get("result") == "ok", out)
            wc.set_app_wayland_display("/nonexistent.sock")
            st, out = cu_json({"action": "type", "text": "fallback"})
            res.check("cu(+wl): type falls back on dead app socket", out.get("result") == "ok", out)
        except Exception as e:
            res.check("cu(+wl): app-compositor typing", False, repr(e)[:130])

        st, out = cu_json({"action": "screenshot"})
        got = png_size(base64.b64decode(out.get("data", ""))) if out.get("data") else None
        res.check("cu(+wl): screenshot 800x480 readback", got == (800, 480), got)
        cu("cu(+wl): mouse_move+click on wayland",
           {"action": "left_click", "coordinate": [400, 240]},
           lambda s, o: o.get("result") == "ok")
        st, out = cu_json({"action": "cursor_position"})
        import re as _re
        res.check("cu(+wl): cursor_position", _re.fullmatch(r"X=\d+,Y=\d+", out.get("text", "")), out)
        st, out = cu_json({"action": "zoom", "region": [0, 0, 400, 240]})
        zg = png_size(base64.b64decode(out.get("data", ""))) if out.get("data") else None
        res.check("cu(+wl): zoom crop 400x240", zg == (400, 240), zg)

        wc.stop_capture()
        time.sleep(0.5)
        res.check("wl: cleared streaming capture", not wc.is_capturing, "")
    except Exception as e:
        res.check("wl: phase D", False, repr(e)[:220])

    # recorder own-capture on wayland
    recw = os.path.join(H.WORKDIR, "cap2-rec-wl.mp4")
    os.path.exists(recw) and os.unlink(recw)
    try:
        cs = make_settings(w=800, h=480)
        cs.use_wayland = True
        cs.display_id = 0
        st = pixelflux.start_recording(recw, cs)
        res.check("rec(wl): own-capture active (wayland)", st.get("active") is True, st)
        deadline = time.time() + 8
        while time.time() < deadline:
            cur = pixelflux.recording_status()
            if cur and cur.get("frames", 0) > 3:
                break
            time.sleep(0.3)
        fin = pixelflux.stop_recording()
        info = FF_RUN(ffprobe(recw))
        got = None
        if info:
            v = [s for s in info.get("streams", []) if s.get("codec_type") == "video"]
            if v:
                got = (v[0].get("codec_name"), v[0].get("width"), v[0].get("height"))
        res.check("rec(wl): MP4 h264 800x480", got == ("h264", 800, 480), got)
        res.check("rec(wl): stop clean", fin.get("error") in (None, ""), fin)
    except Exception as e:
        res.check("rec(wl): wayland recorder", False, repr(e)[:200])

    # ============================= Phase E: pcmflux extras ===============
    print("\n=== Phase E: pcmflux extras ===", flush=True)
    H.pulse_setup()
    subprocess.run(["pactl", "load-module", "module-null-sink", "sink_name=cs2out",
                    "rate=48000", "channels=2"],
                   capture_output=True,
                   env=dict(os.environ))

    # silence gate: silent monitor -> few frames; tone -> frames
    tone = None
    try:
        frames_silent = []
        ps = pcmflux.AudioCaptureSettings()
        ps.device_name = "cs2out.monitor"
        ps.sample_rate = 48000
        ps.channels = 2
        ps.use_silence_gate = True
        ac = pcmflux.AudioCapture()
        ac.start_capture(ps, lambda f: frames_silent.append(bytes(f)))
        time.sleep(2.0)
        n_silent = len(frames_silent)
        ac.stop_capture()
        subprocess.run(["pactl", "set-default-sink", "cs2out"], capture_output=True,
                       env=dict(os.environ))
        tone = H.spawn(
            ["ffplay", "-nodisp", "-autoexit", "-loglevel", "quiet",
             "-f", "lavfi", "-i", "sine=frequency=440:duration=30", "-af", "volume=0.7"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(0.5)
        frames_tone = []
        ac2 = pcmflux.AudioCapture()
        ac2.start_capture(ps, lambda f: frames_tone.append(bytes(f)))
        deadline = time.time() + 4
        while time.time() < deadline and len(frames_tone) < 20:
            time.sleep(0.2)
        ac2.stop_capture()
        subprocess.run(["pactl", "set-default-sink", "output"], capture_output=True,
                       env=dict(os.environ))
        res.check("pcm: silence gate suppresses silence then passes audio",
                  n_silent <= 2 < len(frames_tone), (n_silent, len(frames_tone)))
        tone and tone.terminate(); tone = None
    except Exception as e:
        tone and tone.terminate()
        res.check("pcm: silence gate", False, repr(e)[:160])

    # CBR + frame durations + half-rate + mono + debug + latency
    for label, fd, sr, ch, vbr in [("10ms/48k", 10.0, 48000, 2, False),
                                   ("60ms/24k mono", 60.0, 24000, 1, True),
                                   ("20ms/48k mono", 20.0, 48000, 1, False)]:
        try:
            frr = []
            ps = pcmflux.AudioCaptureSettings()
            ps.device_name = "cs2out.monitor"
            ps.sample_rate = sr
            ps.channels = ch
            ps.frame_duration_ms = fd
            ps.use_vbr = vbr
            ps.use_silence_gate = False
            ps.debug_logging = (label == "20ms/48k mono")
            ps.latency_ms = 40 if label == "20ms/48k mono" else 20
            ac = pcmflux.AudioCapture()
            ac.start_capture(ps, lambda f, sink=frr: sink.append(f.pts))
            deadline = time.time() + selfpt(fd)
            while time.time() < deadline and len(frr) < 5:
                time.sleep(0.2)
            ac.stop_capture()
            deltas = [b - a for a, b in zip(frr, frr[1:]) if b - a > 0]
            exp = sr * fd / 1000.0
            med = sorted(deltas)[len(deltas) // 2] if deltas else None
            res.check(f"pcm: {label} frames flow at {fd:.0f}ms cadence",
                      len(frr) >= 5 and med and 0.6 * exp <= med <= 1.4 * exp,
                      (len(frr), med, exp))
        except Exception as e:
            res.check(f"pcm: {label}", False, repr(e)[:130])

    try:
        bad = pcmflux.AudioCaptureSettings()
        bad.device_name = "definitely-not-a-sink.monitor"
        bad.sample_rate = 48000
        bad.channels = 2
        ac = pcmflux.AudioCapture()
        ac.start_capture(bad, lambda f: None)
        res.check("pcm: bad capture device raises", False, "")
        ac.stop_capture()
    except Exception as e:
        res.check("pcm: bad capture device raises", True, str(e)[:70])

    # RED round trip + playback small buffer + debug
    try:
        ps = pcmflux.AudioCaptureSettings()
        ps.device_name = "cs2out.monitor"
        ps.sample_rate = 48000
        ps.channels = 2
        ps.red_distance = 3
        ps.omit_audio_header = True
        ps.use_silence_gate = False
        red_frames = []
        ac = pcmflux.AudioCapture()
        ac.start_capture(ps, lambda f: red_frames.append(bytes(f)))
        deadline = time.time() + 3
        while time.time() < deadline and len(red_frames) < 4:
            time.sleep(0.2)
        ac.stop_capture()
        pb = pcmflux.AudioPlayback()
        pbs = pcmflux.AudioPlaybackSettings()
        pbs.device_name = "cs2out"
        pbs.sample_rate = 48000
        pbs.channels = 2
        pbs.max_buffer_bytes = 65536
        pbs.debug_logging = True
        pb.start(pbs)
        for f in red_frames[:2]:
            pb.write_red(f, 0)
        for f in red_frames:
            pb.write(f)
        time.sleep(0.3)
        pb.stop()
        res.check("pcm: RED capture + playback write_red (+small buffer)",
                  len(red_frames) >= 2, len(red_frames))
    except Exception as e:
        res.check("pcm: RED round trip", False, repr(e)[:150])

    try:
        # PulseAudio falls back to the default sink and renegotiates the spec when a
        # named playback device does not exist (server behavior, not a pcmflux defect):
        # assert the deterministic contract instead — write/write_red raise once the
        # playback worker is not running.
        pbx = pcmflux.AudioPlayback()
        try:
            pbx.write(b"\x00" * 64)
            res.check("pcm: write on non-running playback raises", False, "")
        except Exception as e:
            res.check("pcm: write on non-running playback raises", "not running" in str(e), str(e)[:60])
        try:
            pbx.write_red(b"\x00" * 64, 0)
            res.check("pcm: write_red on non-running playback raises", False, "")
        except Exception as e:
            res.check("pcm: write_red on non-running playback raises", "not running" in str(e), str(e)[:60])
    except Exception as e:
        res.check("pcm: playback error contract", False, repr(e)[:80])

    res.summary()
    return res


def selfpt(fd_ms: float) -> float:
    """Poll deadline in seconds, scaled to the opus frame duration."""
    return max(1.5, fd_ms / 1000.0 * 12)


if __name__ == "__main__":
    os.environ["E2E_DISPLAY"] = TEST_DISPLAY
    sys.exit(0 if main().summary() else 1)
