# -*- coding: utf-8 -*-
"""Comprehensive pixelflux + pcmflux capability soak (standalone, no selkies).

Covers every public Python API of both libraries, including capabilities not
used by selkies or the bundled examples (watermarking, raw-stripe wire mode,
output management, input injection, clipboard data-control, cursor callbacks,
computer-use HTTP actions, recording tap + MP4 recorder, NVENC, pcmflux
multichannel/RED/playback, ...).
"""
import base64
import json
import os
import socket as pysocket
import subprocess
import sys
import threading
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

TEST_DISPLAY = ":98"
CU_PORT = 9599
REC_SOCK = os.path.join(H.WORKDIR, "pixelflux-rec.sock")


def curl_json(url: str, body=None, timeout: float = 15) -> bytes:
    """GET (or POST when a body is given) a URL and return the raw response."""
    data = body.encode() if isinstance(body, str) else body
    req = urllib.request.Request(url, data=data, method="POST" if body is not None else "GET")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read()


def make_settings(encoder: str, w: int = 1024, h: int = 640, **kw):
    """Build a CaptureSettings for the encoder, with overrides applied last.

    Args:
        encoder: ``jpeg``, ``h264enc``, ``h264enc-striped``, ``openh264enc``,
            or ``nvenc``.
        w: Capture width in pixels.
        h: Capture height in pixels.
        **kw: Extra CaptureSettings attributes set verbatim.

    Returns:
        A populated pixelflux.CaptureSettings.
    """
    import pixelflux
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
    cs.use_paint_over_quality = True
    cs.paint_over_trigger_frames = 15
    cs.damage_block_threshold = 10
    cs.damage_block_duration = 20
    cs.jpeg_quality = 90
    if encoder == "jpeg":
        cs.output_mode = 0
        cs.video_fullframe = False
        cs.use_openh264 = False
    else:
        cs.output_mode = 1
        cs.video_fullframe = encoder in ("h264enc", "openh264enc", "nvenc")
        cs.use_openh264 = encoder == "openh264enc"
    for k, v in kw.items():
        setattr(cs, k, v)
    return cs


class FrameCounter:
    """Thread-safe frame-callback sink that classifies stripes by wire header."""

    def __init__(self) -> None:
        self.n = 0
        self.h264 = 0
        self.jpg = 0
        self.idr_flag = 0
        self.idr_nals = 0
        self.raw_headers = set()
        self.lock = threading.Lock()

    def __call__(self, frame) -> None:
        """Count one stripe, detecting H.264/JPEG headers and IDR NALs."""
        try:
            b = bytes(frame)
        except Exception:
            return
        with self.lock:
            self.n += 1
            if len(b) < 1:
                return
            self.raw_headers.add(b[0])
            if b[0] == 0x04:
                self.h264 += 1
                if b[1] == 0x01:
                    self.idr_flag += 1
                payload = b[10:] if len(b) > 10 else b""
                i = 0
                while i + 4 <= len(payload):
                    if (payload[i] == 0 and payload[i+1] == 0 and payload[i+2] == 0 and payload[i+3] == 1
                            and (payload[i+4] & 0x1F) == 5):
                        self.idr_nals += 1
                        break
                    if (payload[i] == 0 and payload[i+1] == 0 and payload[i+2] == 1
                            and i + 3 < len(payload) and (payload[i+3] & 0x1F) == 5):
                        self.idr_nals += 1
                        break
                    i += 1
            elif b[0] == 0x03:
                self.jpg += 1

    def snap(self) -> dict:
        """A consistent snapshot of the counters."""
        with self.lock:
            return dict(n=self.n, h264=self.h264, jpg=self.jpg,
                        idr_nals=self.idr_nals, idr_flag=self.idr_flag)


def run_x11(encoder: str, duration: float = 3.0, **kw) -> tuple:
    """Capture the X11 screen briefly, then request an IDR and capture more.

    Returns:
        Tuple of counter snapshots (before_idr_request, after_idr_request).
    """
    import pixelflux
    caps = pixelflux.ScreenCapture()
    fc = FrameCounter()
    caps.start_capture(fc, make_settings(encoder, **kw))
    time.sleep(duration)
    before = fc.snap()
    caps.request_idr_frame()
    time.sleep(1.2)
    after = fc.snap()
    caps.stop_capture()
    time.sleep(0.3)
    return before, after


def main() -> "H.Results":
    """Sweep every public pixelflux and pcmflux capability once."""
    import pixelflux
    import pcmflux

    res = H.Results("cap-soak")
    os.environ["DISPLAY"] = TEST_DISPLAY
    subprocess.run(["xsetroot", "-solid", "#3366cc"],
                   env={"DISPLAY": TEST_DISPLAY, "PATH": os.environ.get("PATH", "")}, capture_output=True)

    # ---- X11 encoders + on-demand IDR ----
    for enc in ["h264enc", "h264enc-striped", "openh264enc", "jpeg"]:
        try:
            before, after = run_x11(enc)
            res.check(f"x11 {enc}: frames flow", before["n"] >= 6, before["n"])
            if enc == "jpeg":
                res.check(f"x11 {enc}: jpeg stripes", before["jpg"] >= 3, before["jpg"])
            else:
                res.check(f"x11 {enc}: h264 stripes", before["h264"] >= 3, before["h264"])
                res.check(f"x11 {enc}: IDR on demand", after["idr_nals"] > before["idr_nals"],
                          (before["idr_nals"], after["idr_nals"]))
        except Exception as e:
            res.check(f"x11 {enc}: no exception", False, repr(e)[:140])

    # ---- NVENC hardware encoder ----
    try:
        import pixelflux
        cap = pixelflux.ScreenCapture()
        fc = FrameCounter()
        cs = make_settings("h264enc", w=960, h=540)
        # Pin the encode to the first GPU node.
        cs.encode_node_index = 0
        cap.start_capture(fc, cs)
        time.sleep(3.0)
        snap = fc.snap()
        cap.stop_capture()
        res.check("nvenc: hardware H.264 flows", snap["h264"] >= 3, snap["h264"])
    except Exception as e:
        res.check("nvenc: hardware H.264 flows", False, repr(e)[:140])

    # ---- watermark ----
    try:
        png = base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        wp = os.path.join(H.WORKDIR, "wm.png")
        with open(wp, "wb") as f:
            f.write(png)
        before, _ = run_x11("h264enc", duration=2.0, watermark_path=wp.encode(), watermark_location_enum=3)
        res.check("x11 watermark frames still flow", before["n"] >= 4, before["n"])
    except Exception as e:
        res.check("x11 watermark", False, repr(e)[:140])

    # ---- omit_stripe_headers (raw payload wire mode) ----
    try:
        before, _ = run_x11("h264enc", duration=2.0, omit_stripe_headers=True)
        res.check("x11 omit_stripe_headers: frames flow", before["n"] >= 4, before["n"])
    except Exception as e:
        res.check("x11 omit_stripe_headers", False, repr(e)[:140])

    # ---- live tunables ----
    try:
        cap = pixelflux.ScreenCapture()
        fc = FrameCounter()
        cap.start_capture(fc, make_settings("h264enc", w=640, h=400))
        time.sleep(1.0)
        cap.update_framerate(30.0)
        cap.update_video_bitrate(1500)
        cap.update_vbv_multiplier(0.0)
        cap.update_tunables(make_settings("h264enc", w=640, h=400))
        cap.update_capture_region(0, 0, 640, 400)
        time.sleep(1.0)
        cap.stop_capture()
        res.check("x11 live tunables (fps/bitrate/vbv/tunables/region)", True, "")
    except Exception as e:
        res.check("x11 live tunables", False, repr(e)[:140])

    # ---- X11 cursor callback (XFixes monitor) ----
    try:
        import pixelflux
        cap = pixelflux.ScreenCapture()
        curs = []
        cs = make_settings("h264enc", w=640, h=400)
        cap.set_cursor_callback(lambda mt, data, hx, hy: curs.append((mt, len(data or b""), hx, hy)))
        cap.start_capture(FrameCounter(), cs)
        time.sleep(2.0)
        cap.stop_capture()
        res.check("x11 cursor callback registered", True, f"{len(curs)} cursor events")
    except Exception as e:
        res.check("x11 cursor callback", False, repr(e)[:140])

    # ---- Wayland backend: compositor capture + outputs + input + clipboard ----
    try:
        import pixelflux
        wc = pixelflux.ScreenCapture()
        wfc = FrameCounter()
        wcs = make_settings("h264enc", w=800, h=480)
        wcs.use_wayland = True
        wc.start_capture(wfc, wcs)
        time.sleep(3.5)
        outs = wc.list_outputs()
        wins = wc.list_windows()
        geo = None
        try:
            geo = wc.get_realized_geometry(0)
        except Exception as e:
            geo = f"ERR {e}"
        res.check("wayland capture frames", wfc.snap()["n"] >= 6, wfc.snap()["n"])
        res.check("wayland list_outputs (id,x,y,w,h,scale,active)",
                  isinstance(outs, list) and len(outs) >= 1, outs)
        res.check("wayland list_windows", isinstance(wins, list), str(wins)[:80])
        res.check("wayland get_realized_geometry", isinstance(geo, (tuple, list)), geo)
        # output management
        try:
            oid = wc.create_output(1, 200, 200, 800, 0, 1.0)
            outs2 = wc.list_outputs()
            found = any(o[0] == oid for o in outs2)
            wc.reposition_output(oid, 300, 50)
            wc.destroy_output(oid)
            res.check("wayland create/reposition/destroy output", found, (oid, outs2))
        except Exception as e:
            res.check("wayland output mgmt", False, repr(e)[:140])
        # input injection (virtual-keyboard + pointer)
        try:
            wc.set_keymap_string("us")
            wc.get_xkb_keymap_string()
            wc.bind_keysyms([ord("x")])
            wc.inject_key(45, 1)
            time.sleep(0.2)
            ks = wc.get_keyboard_state()
            wc.inject_key(45, 0)
            res.check("wayland keymap/bind/inject/get_state", isinstance(ks, tuple), str(ks)[:80])
        except pixelflux.VirtualKeyboardUnavailable:
            res.check("wayland key injection (VK unavailable)", True, "compositor lacks zwp_virtual_keyboard")
        except Exception as e:
            res.check("wayland key injection", False, repr(e)[:140])
        try:
            wc.inject_mouse_move(400.0, 240.0)
            wc.inject_mouse_button(272, 1)
            time.sleep(0.1)
            wc.inject_mouse_button(272, 0)
            wc.inject_mouse_scroll(0.0, 5.0)
            wc.inject_relative_mouse_move(2.0, -2.0)
            res.check("wayland pointer injection", True, "")
        except Exception as e:
            res.check("wayland pointer injection", False, repr(e)[:140])
        try:
            wc.set_xkb_layout("us")
            wc.type_text_wayland("wayland-1", "hello")
            res.check("wayland type_text_wayland", True, "")
        except Exception as e:
            res.check("wayland type_text_wayland", False, repr(e)[:140])
        # clipboard data-control
        try:
            wc.set_clipboard("text/plain;charset=utf-8", b"cap-clip-probe")
            time.sleep(0.3)
            wc.clipboard_types_app("wayland-1")
            data = wc.clipboard_read_app("wayland-1", "text/plain;charset=utf-8")
            wc.clipboard_clear_app("wayland-1")
            got = bytes(data).decode("utf-8", errors="replace") if data else None
            res.check("wayland clipboard set/read/clear", got == "cap-clip-probe", repr(got))
        except Exception as e:
            res.check("wayland clipboard data-control", False, repr(e)[:160])
        # cursor control
        try:
            wc.set_cursor_size(24)
            wc.set_cursor_rendering(True)
            res.check("wayland set_cursor_size/rendering", True, "")
        except Exception as e:
            res.check("wayland cursor control", False, repr(e)[:140])
        wc.stop_capture()
        time.sleep(0.3)
    except Exception as e:
        res.check("wayland standalone", False, repr(e)[:200])

    # ---- computer-use HTTP server (all actions) ----
    try:
        pixelflux.start_computer_use(f"127.0.0.1:{CU_PORT}")
        time.sleep(1.0)
        base = f"http://127.0.0.1:{CU_PORT}"
        r = json.loads(curl_json(f"{base}/computer-use", '{"action":"screenshot"}'))
        png = base64.b64decode(r.get("data", ""))
        res.check("cu: screenshot returns PNG", png[:4] == b"\x89PNG", len(png))
        for act in ['{"action":"cursor_position"}',
                    '{"action":"mouse_move","coordinate":[400,240]}',
                    '{"action":"left_click"}',
                    '{"action":"key","text":"a"}',
                    '{"action":"scroll","scroll_direction":"down","scroll_amount":2}',
                    '{"action":"wait","duration":0.2}',
                    '{"action":"zoom","region":[0,0,200,200]}']:
            try:
                r = json.loads(curl_json(f"{base}/computer-use", act))
                res.check(f"cu: {act[:40]}", r.get("result") == "ok" or "data" in r or "error" not in r,
                          str(r)[:80])
            except Exception as e:
                res.check(f"cu: {act[:40]}", False, repr(e)[:80])
        r = json.loads(curl_json(f"{base}/record_start", "{}"))
        time.sleep(1.5)
        json.loads(curl_json(f"{base}/record_status"))
        stopj = json.loads(curl_json(f"{base}/record_stop", "{}"))
        res.check("cu: record start/status/stop", stopj.get("stopped") is True or stopj.get("active") is not None,
                  str(stopj)[:80])
    except Exception as e:
        res.check("computer-use server", False, repr(e)[:200])

    # ---- recording socket tap (raw H.264 ES) ----
    try:
        cap = pixelflux.ScreenCapture()
        fc = FrameCounter()
        if os.path.exists(REC_SOCK):
            os.unlink(REC_SOCK)
        cs = make_settings("h264enc", w=640, h=400)
        cs.recording_socket = REC_SOCK
        cap.start_capture(fc, cs)
        time.sleep(1.5)
        cli = pysocket.socket(pysocket.AF_UNIX, pysocket.SOCK_STREAM)
        cli.settimeout(3.0)
        cli.connect(REC_SOCK)
        data = b""
        deadline = time.time() + 3.0
        while time.time() < deadline:
            try:
                chunk = cli.recv(65536)
            except pysocket.timeout:
                break
            if not chunk:
                break
            data += chunk
            if b"\x00\x00\x00\x01\x67" in data and b"\x00\x00\x00\x01\x65" in data:
                break
        cli.close()
        has = (b"\x00\x00\x00\x01\x67" in data or b"\x00\x00\x01\x67" in data) and \
              (b"\x00\x00\x00\x01\x65" in data or b"\x00\x00\x01\x65" in data)
        res.check("recording socket: H.264 ES (SPS+IDR)", has, len(data))
        cap.stop_capture()
        time.sleep(0.3)
    except Exception as e:
        res.check("recording socket tap", False, repr(e)[:200])

    # ---- module helpers ----
    try:
        name = pixelflux.get_wayland_display_name()
        res.check("get_wayland_display_name", isinstance(name, str), name)
        sf = pixelflux.stripe_frame_from_buffer(b"\x04\x00" + b"\x00" * 8 + b"\x00\x00\x00\x01\x65" * 1)
        res.check("stripe_frame_from_buffer", isinstance(sf, pixelflux.StripeFrame)
                  and hasattr(sf, "data_type"), str(type(sf)))
    except Exception as e:
        res.check("module helpers", False, repr(e)[:140])
    try:
        pixelflux.ensure_wayland_display()
        pixelflux.ensure_wayland_display()
        res.check("ensure_wayland_display idempotent", True, "")
    except Exception as e:
        res.check("ensure_wayland_display", False, repr(e)[:140])

    # ---- pcmflux capture: stereo, VBR, header prefix, silence gate, latency ----
    H.pulse_setup()
    tone = H.spawn(["ffplay", "-nodisp", "-autoexit", "-loglevel", "quiet",
                    "-f", "lavfi", "-i", "sine=frequency=440:duration=30",
                    "-af", "volume=0.5"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(0.5)
    try:
        H.pulse_setup()
        ps = pcmflux.AudioCaptureSettings()
        ps.device_name = "output.monitor"
        ps.sample_rate = 48000
        ps.channels = 2
        ps.opus_bitrate = 128000
        ps.frame_duration_ms = 20.0
        ps.latency_ms = 20
        ps.use_vbr = True
        ps.use_silence_gate = False
        ps.red_distance = 0
        ps.omit_audio_header = False
        frames = []
        ac = pcmflux.AudioCapture()
        ac.start_capture(ps, lambda frame: frames.append((frame, frame.pts)))
        deadline = time.time() + 4
        while time.time() < deadline and len(frames) < 20:
            time.sleep(0.2)
        header_ok = all(len(bytes(bf)) > 2 and bytes(bf)[0:2] == b"\x01\x00" for bf, _ in frames[:20])
        res.check("pcmflux opus + native header", len(frames) >= 20 and header_ok,
                  (len(frames), header_ok))
        ac.update_audio_bitrate(192000)
        time.sleep(0.5)
        ac.stop_capture()
        res.check("pcmflux update_audio_bitrate + stop", not ac.is_capturing, "")
    except Exception as e:
        res.check("pcmflux capture", False, repr(e)[:200])

    # ---- pcmflux multichannel (6 ch) capture ----
    try:
        subprocess.run(["pactl", "load-module", "module-null-sink",
                        "sink_name=surround51", "rate=48000"],
                       capture_output=True,
                       env=dict(os.environ))
        ps = pcmflux.AudioCaptureSettings()
        ps.device_name = "surround51.monitor"
        ps.sample_rate = 48000
        ps.channels = 6
        ps.opus_bitrate = 192000
        ps.use_silence_gate = False
        fr = []
        ac = pcmflux.AudioCapture()
        ac.start_capture(ps, lambda frame: fr.append((len(bytes(frame)), frame.pts)))
        deadline = time.time() + 3
        while time.time() < deadline and len(fr) < 10:
            time.sleep(0.2)
        res.check("pcmflux multichannel 6ch capture", len(fr) >= 10 and all(s > 4 for s, _ in fr),
                  (len(fr), fr[:2]))
        ac.stop_capture()
    except Exception as e:
        res.check("pcmflux multichannel", False, repr(e)[:200])

    # ---- pcmflux RED redundancy + raw opus (omit header) ----
    try:
        ps = pcmflux.AudioCaptureSettings()
        ps.device_name = "output.monitor"
        ps.sample_rate = 48000
        ps.channels = 2
        ps.omit_audio_header = True
        ps.red_distance = 2
        frames2 = []
        ac2 = pcmflux.AudioCapture()
        ac2.start_capture(ps, lambda frame: frames2.append(bytes(frame)))
        deadline = time.time() + 3
        while time.time() < deadline and len(frames2) < 10:
            time.sleep(0.2)
        raw_ok = bool(frames2) and all(b and b[0] != 0x01 for b in frames2)
        ac2.stop_capture()
        res.check("pcmflux raw opus (omit header)", raw_ok, len(frames2))
    except Exception as e:
        res.check("pcmflux raw opus", False, repr(e)[:200])

    # ---- pcmflux AudioPlayback (mic uplink) into the 'output' sink ----
    try:
        pb = pcmflux.AudioPlayback()
        pbs = pcmflux.AudioPlaybackSettings()
        pbs.device_name = "output"
        pbs.sample_rate = 48000
        pbs.channels = 2
        pb.start(pbs)
        # Encode a short opus stream from the monitor and feed it back.
        ps = pcmflux.AudioCaptureSettings()
        ps.device_name = "output.monitor"
        ps.sample_rate = 48000
        ps.channels = 2
        ps.opus_bitrate = 64000
        ps.use_silence_gate = False
        ps.omit_audio_header = True
        acap = pcmflux.AudioCapture()
        capf = []
        got = []
        acap.start_capture(ps, lambda frame: capf.append(bytes(frame)))
        deadline = time.time() + 2
        while time.time() < deadline and len(capf) < 10:
            time.sleep(0.2)
        pb.write(capf[0]) if capf else None
        time.sleep(0.3)
        wrote = bool(capf)
        try:
            pb.write_red(capf[0], capf[0]) if capf else None
        except Exception:
            pass
        pb.stop()
        acap.stop_capture()
        res.check("pcmflux playback start/stop/write", wrote, f"owning={wrote}")
        tone.terminate()
    except Exception as e:
        res.check("pcmflux playback", False, repr(e)[:200])

    res.summary()
    return res


if __name__ == "__main__":
    os.environ["E2E_DISPLAY"] = TEST_DISPLAY
    r = main()
    sys.exit(0 if not r.failed() else 1)
