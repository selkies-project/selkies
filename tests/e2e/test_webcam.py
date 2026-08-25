#!/usr/bin/env python3
"""The webcam uplink end to end: a browser's camera, switched on through the
dashboard's pipeline control, must come out of the virtual V4L2 device on the
server — over the WebSocket (WebCodecs frames) and over WebRTC (the camera
track on the reserved sendonly transceiver) — and switch off again. Chromium
captures a known-colour y4m file as its fake camera, so the device's pixels
are checked, not just their presence; Firefox's synthetic fake camera proves
the `<video>` source (it has no ``MediaStreamTrackProcessor``, so its frames
are sampled from an element and encoded by the codec the worker measured) and
the Firefox WebRTC path deliver frames.
The operator lock (``webcam_enabled=false|locked``) must withhold the uplink
on both transports.

    python3 tests/e2e/test_webcam.py websockets|webrtc|locked
"""
import os
import subprocess
import sys
import tempfile
import time
from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_browsers as TB
from playwright.sync_api import sync_playwright

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ADDON = os.path.join(ROOT, "addons", "v4l2-interposer")
TOOLS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "tools")
INTERPOSER = os.path.join(ADDON, "selkies_v4l2_interposer.so")
PROBE = os.path.join(TOOLS, "v4l2probe")

# The I420 samples the fake camera's y4m carries; the camera is limited-range
# video, so they travel unchanged and the encoders land within a few steps.
GREEN = (150, 44, 21)

CAM_JS = """
  window.__camStatus = [];
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || !d.type) return;
    if ((d.type === 'sidebarButtonStatusUpdate' || d.type === 'pipelineStatusUpdate') && d.webcam !== undefined) {
      window.__camStatus.push(!!d.webcam);
    }
  });
"""


def build() -> None:
    subprocess.run(["make", "-C", ADDON], check=True, stdout=subprocess.DEVNULL)
    subprocess.run(["make", "-C", TOOLS, "v4l2probe"], check=True, stdout=subprocess.DEVNULL)


def green_y4m(path: str, width: int = 640, height: int = 480, frames: int = 30) -> None:
    """A solid green I420 y4m clip for Chromium's --use-file-for-fake-video-capture."""
    y = bytes([150]) * (width * height)
    u = bytes([44]) * (width * height // 4)
    v = bytes([21]) * (width * height // 4)
    with open(path, "wb") as f:
        f.write(f"YUV4MPEG2 W{width} H{height} F30:1 Ip A1:1 C420jpeg\n".encode())
        for _ in range(frames):
            f.write(b"FRAME\n" + y + u + v)


# Two flat halves in one I420 frame: a turn relayed on the wire has to swap them
# in the device picture, which a solid clip could never show.
LEFT_HALF = (150, 44, 21)
RIGHT_HALF = (80, 200, 120)


def split_y4m(path: str, width: int = 640, height: int = 480, frames: int = 30) -> None:
    """A y4m split down the middle, for Chromium's --use-file-for-fake-video-capture."""
    def row(values, count):
        half = count // 2
        return bytes([values[0]]) * half + bytes([values[1]]) * (count - half)
    y = row((LEFT_HALF[0], RIGHT_HALF[0]), width) * height
    u = row((LEFT_HALF[1], RIGHT_HALF[1]), width // 2) * (height // 2)
    v = row((LEFT_HALF[2], RIGHT_HALF[2]), width // 2) * (height // 2)
    with open(path, "wb") as f:
        f.write(f"YUV4MPEG2 W{width} H{height} F30:1 Ip A1:1 C420jpeg\n".encode())
        for _ in range(frames):
            f.write(b"FRAME\n" + y + u + v)


# A half turn stamped onto every uplink frame's flags byte, which is what a client
# on an engine that hands the camera's own orientation over sends.
HALF_TURN_JS = """
  (() => {
    const send = WebSocket.prototype.send;
    WebSocket.prototype.send = function (data) {
      if (data instanceof ArrayBuffer && data.byteLength > 3) {
        const b = new Uint8Array(data);
        if (b[0] === 0x06) b[2] = (b[2] & ~0x06) | (2 << 1);
      }
      return send.apply(this, arguments);
    };
  })();
"""


def probe(frames: int, timeout_ms: int = 4000, samples=((320, 240),)) -> dict:
    env = dict(os.environ, LD_PRELOAD=INTERPOSER, SELKIES_WEBCAM_SOCKET_PATH=os.environ.get("SELKIES_WEBCAM_SOCKET_PATH", "/tmp"))
    cmd = [PROBE, "--timeout", str(timeout_ms)]
    for x, y in samples:
        cmd += ["--sample", f"{x},{y}"]
    cmd += ["/dev/video0", str(frames)]
    try:
        p = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=60)
    except subprocess.TimeoutExpired:
        return {"rc": -1, "error": "timeout"}
    out = {"rc": p.returncode, "samples": {}}
    for line in p.stdout.splitlines():
        if "=" not in line:
            continue
        k, v = line.split("=", 1)
        if k == "sample":
            pos, yuv = v.split(":")
            out["samples"][tuple(int(n) for n in pos.split(","))] = tuple(int(n) for n in yuv.split(","))
        else:
            out[k] = v
    return out


# Long enough for the floor between the checks that ask whether anything still reads the
# camera (webcam.REFORMAT_RECHECK_SECONDS) to pass, plus a frame to act on the answer.
REFORMAT_SETTLE = 8.0


def device_format() -> str:
    """The device's current pixel format, as the interposer reports it."""
    return str(probe(1, timeout_ms=2000).get("format", ""))


def wait_format(fourcc: str, timeout: float = 20) -> bool:
    """Poll until the device reports `fourcc`. The probe is itself a reader, so the gaps
    between polls are what leave the server free to re-create the device."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if device_format() == fourcc:
            return True
        time.sleep(2)
    return False


def start_reader() -> subprocess.Popen:
    """An interposer client that holds the device open until it is terminated."""
    env = dict(os.environ, LD_PRELOAD=INTERPOSER,
               SELKIES_WEBCAM_SOCKET_PATH=os.environ.get("SELKIES_WEBCAM_SOCKET_PATH", "/tmp"))
    return subprocess.Popen([PROBE, "--timeout", "60000", "/dev/video0", "100000"],
                            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def near(got, want, tol=12) -> bool:
    return got is not None and all(abs(g - w) <= tol for g, w in zip(got, want))


def toggle(page, enabled: bool) -> None:
    page.evaluate(f"window.postMessage({{type: 'pipelineControl', pipeline: 'webcam', enabled: {str(enabled).lower()}}}, window.location.origin)")


def wait_status(page, value: bool, timeout: float = 20) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        statuses = page.evaluate("window.__camStatus")
        if statuses and statuses[-1] == value:
            return True
        time.sleep(0.25)
    return False


# Every WebCodecs global removed, the way an engine without the API presents;
# the uplink then takes the JPEG rung and the screen arrives as striped JPEG.
NO_WEBCODECS_JS = """
  (() => {
    for (const name of ['VideoDecoder', 'VideoEncoder', 'VideoFrame', 'EncodedVideoChunk', 'ImageDecoder']) {
      try { Object.defineProperty(window, name, { value: undefined, configurable: true, writable: true }); } catch (e) {}
    }
  })();
"""


def launch(p, engine: str, y4m: str, mode: str, init_js: Optional[str] = None):
    if engine == "firefox":
        prefs = {
            "media.navigator.permission.disabled": True,
            "media.navigator.streams.fake": True,
            "media.autoplay.default": 0,
            "media.autoplay.blocking_policy": 0,
            "media.autoplay.block-webaudio": False,
            "media.gmp-gmpopenh264.enabled": True,
            **TB.openh264_prefs(),
        }
        if TB.openh264_version():
            # The side-loaded OpenH264 lives in the persistent e2e profile; with it
            # Firefox plays the session's H.264 and the WebRTC uplink can be driven.
            ctx = p.firefox.launch_persistent_context(user_data_dir=TB.FF_E2E_PROFILE, headless=True,
                                                      viewport={"width": 1280, "height": 720}, firefox_user_prefs=prefs)
            browser = ctx.browser or ctx
        else:
            kw = {"headless": True, "firefox_user_prefs": prefs}
            if C.FIREFOX_PATH:
                kw["executable_path"] = C.FIREFOX_PATH
            browser = p.firefox.launch(**kw)
            ctx = browser.new_context(viewport={"width": 1280, "height": 720})
    else:
        # The headless shell has no media capture; the full Chromium build (new
        # headless mode) or the system Chrome named by E2E_CHROME is needed.
        args = C.BROWSER_ARGS + ["--use-fake-device-for-media-stream", f"--use-file-for-fake-video-capture={y4m}"]
        kw = {"headless": True, "args": args}
        if C.CHROME_PATH:
            kw["executable_path"] = C.CHROME_PATH
        else:
            kw["channel"] = "chromium"
        browser = p.chromium.launch(**kw)
        ctx = browser.new_context(viewport={"width": 1280, "height": 720}, permissions=["camera"])
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(CAM_JS)
    if init_js:
        ctx.add_init_script(init_js)
    page = ctx.new_page()
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(H.BASE_URL + "/", wait_until="load")
    return browser, page, errors


def jpeg_centre(path: str):
    """(size, centre RGB, RGB at x=20 on the centre row) of a dumped JPEG, or None."""
    try:
        from PIL import Image
        img = Image.open(path).convert("RGB")
        return img.size, img.getpixel((img.width // 2, img.height // 2)), img.getpixel((20, img.height // 2))
    except Exception:
        return None


def nowebcodecs_block() -> "H.Results":
    """Chromium with every WebCodecs global removed: the screen degrades to striped
    JPEG, the camera goes up the JPEG rung, and the server's default device format
    follows that uplink into an MJPEG device whose frames carry the camera picture."""
    res = H.Results("webcam-nowebcodecs")
    y4m = os.path.join(tempfile.mkdtemp(prefix="selkies-cam-"), "green.y4m")
    green_y4m(y4m)
    dump = os.path.join(os.path.dirname(y4m), "frame.jpg")
    H.server_start(mode="websockets", wayland=False, extra_env={"SELKIES_WEBCAM_ENABLED": "false"})
    try:
        with sync_playwright() as p:
            browser, page, errors = launch(p, "chromium", y4m, "websockets", init_js=NO_WEBCODECS_JS)
            logs = []
            page.on("console", lambda m: logs.append(m.text) if "[Webcam]" in m.text else None)
            video = C.wait_ws_video(page, timeout=30)
            res.check("screen stream up without WebCodecs", bool(video), str(video)[:100])
            stored = page.evaluate("localStorage.getItem((location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_') + '_encoder')")
            res.check("screen encoder pinned to jpeg", stored == "jpeg", stored)
            toggle(page, True)
            res.check("webcam reports active", wait_status(page, True), str(page.evaluate("window.__camStatus")))
            deadline = time.time() + 25
            while time.time() < deadline and probe(2, timeout_ms=1500).get("rc") != 0:
                time.sleep(0.5)
            env = dict(os.environ, LD_PRELOAD=INTERPOSER, SELKIES_WEBCAM_SOCKET_PATH="/tmp")
            p2 = subprocess.run([PROBE, "--timeout", "4000", "--dump", dump, "/dev/video0", "30"], env=env,
                                capture_output=True, text=True, timeout=40)
            r = dict(line.split("=", 1) for line in p2.stdout.splitlines() if "=" in line and not line.startswith("sample"))
            res.check("30 frames reach /dev/video0", p2.returncode == 0 and r.get("frames") == "30", f"rc={p2.returncode} {r.get('frames')} {r.get('error', '')}")
            res.check("device follows the JPEG uplink: MJPEG at 1280x720",
                      (r.get("format"), r.get("width"), r.get("height")) == ("MJPG", "1280", "720"), f"{r.get('format')} {r.get('width')}x{r.get('height')}")
            res.check("frames flow at camera rate", float(r.get("fps", "0") or 0) >= 12, r.get("fps"))
            px = jpeg_centre(dump)
            res.check("device frame is a 1280x720 JPEG of the green camera, pillarboxed",
                      px is not None and px[0] == (1280, 720) and px[1][1] > 200 and px[1][0] < 60 and px[1][2] < 60
                      and max(px[2]) < 40, str(px))
            res.check("client took the JPEG rung", any("JPEG" in line for line in logs), "; ".join(logs)[:200])
            toggle(page, False)
            res.check("webcam reports inactive", wait_status(page, False), str(page.evaluate("window.__camStatus")))
            res.check("no page errors", not errors, "; ".join(errors)[:200])
            browser.close()
    finally:
        H.server_stop()
    return res


def transport_block(mode: str) -> "H.Results":
    res = H.Results(f"webcam-{mode}")
    y4m = os.path.join(tempfile.mkdtemp(prefix="selkies-cam-"), "green.y4m")
    green_y4m(y4m)
    H.server_start(mode=mode, wayland=False, extra_env={"SELKIES_WEBCAM_ENABLED": "false"})
    try:
        for engine in ("chromium", "firefox"):
            with sync_playwright() as p:
                browser, page, errors = launch(p, engine, y4m, mode)
                video = C.wait_ws_video(page) if mode == "websockets" else C.wait_wr_video(page)
                if not video and engine == "firefox" and mode == "webrtc":
                    # Playwright's Firefox ships no OpenH264, so the session's H.264
                    # stream never plays there; the uplink cannot be driven without it.
                    res.skip("firefox: webrtc webcam uplink", "no H.264 decode in this Firefox build (seed OpenH264 with tests/tools/fetch-openh264.sh)")
                    browser.close()
                    continue
                res.check(f"{engine}: stream up", bool(video), str(video)[:100])
                toggle(page, True)
                res.check(f"{engine}: webcam reports active", wait_status(page, True), str(page.evaluate("window.__camStatus")))
                # The server brings the camera up on the first frame; give it a moment
                # to appear before the measured capture.
                deadline = time.time() + 20
                while time.time() < deadline and probe(2, timeout_ms=1500).get("rc") != 0:
                    time.sleep(0.5)
                r = probe(30, samples=[(640, 360), (20, 20)])
                res.check(f"{engine}: 30 frames reach /dev/video0", r.get("rc") == 0 and r.get("frames") == "30",
                          f"rc={r.get('rc')} frames={r.get('frames')} err={r.get('error', '')}")
                # The process-wide camera came up I420 on chromium's encoded uplink
                # and outlives it; firefox's WebSocket leg is decoded into the same
                # device, whichever codec its encode worker measured as keeping up.
                res.check(f"{engine}: device is the configured 1280x720 I420",
                          (r.get("format"), r.get("width"), r.get("height")) == ("YU12", "1280", "720"),
                          f"{r.get('format')} {r.get('width')}x{r.get('height')}")
                res.check(f"{engine}: frames flow at camera rate", float(r.get("fps", "0")) >= 12, r.get("fps"))
                if engine == "chromium":
                    # The 4:3 green camera sits pillarboxed in the 16:9 device: picture
                    # centre green, the strip at x=20 black.
                    res.check("chromium: centre is the fake camera's green", near(r["samples"].get((640, 360)), GREEN), str(r["samples"]))
                    res.check("chromium: pillarbox is black", near(r["samples"].get((20, 20)), (16, 128, 128)), str(r["samples"]))
                toggle(page, False)
                res.check(f"{engine}: webcam reports inactive", wait_status(page, False), str(page.evaluate("window.__camStatus")))
                # Frames already in flight (RTP jitter buffer, decoder queue) land for a
                # moment after the stop; the device must then go quiet.
                time.sleep(1.5)
                r = probe(10, timeout_ms=3000)
                res.check(f"{engine}: no frames after disable", r.get("rc") != 0 and int(r.get("frames", "0") or 0) < 3, str(r.get("frames")))
                res.check(f"{engine}: no page errors", not errors, "; ".join(errors)[:200])
                browser.close()
    finally:
        H.server_stop()
    return res


def locked_block() -> "H.Results":
    res = H.Results("webcam-locked")
    y4m = os.path.join(tempfile.mkdtemp(prefix="selkies-cam-"), "green.y4m")
    green_y4m(y4m)
    for mode in ("websockets", "webrtc"):
        H.server_start(mode=mode, wayland=False, extra_env={"SELKIES_WEBCAM_ENABLED": "false|locked"})
        try:
            with sync_playwright() as p:
                browser, page, _ = launch(p, "chromium", y4m, mode)
                video = C.wait_ws_video(page) if mode == "websockets" else C.wait_wr_video(page)
                res.check(f"{mode}: stream up", bool(video))
                toggle(page, True)
                time.sleep(4)
                r = probe(3, timeout_ms=2500)
                res.check(f"{mode}: locked-off webcam feeds no frames", r.get("frames", "0") == "0" or r.get("rc") != 0, str(r.get("frames")))
                statuses = page.evaluate("window.__camStatus")
                res.check(f"{mode}: client settles on webcam off", not statuses or statuses[-1] is False, str(statuses))
                browser.close()
        finally:
            H.server_stop()
    return res


def rotation_block() -> "H.Results":
    """What the flags byte says about a frame's orientation is what the device shows.

    The client leaves the transform out of the bitstream and relays it, so the
    picture is only upright if the server bakes it in: the same clip sent with and
    without a half turn must come out of /dev/video0 mirrored against each other.
    """
    res = H.Results("webcam-rotation")
    y4m = os.path.join(tempfile.mkdtemp(prefix="selkies-cam-"), "split.y4m")
    split_y4m(y4m)
    # Well inside each half of the 4:3 picture as it sits pillarboxed in the device.
    left_at, right_at = (400, 360), (880, 360)
    H.server_start(mode="websockets", wayland=False, extra_env={"SELKIES_WEBCAM_ENABLED": "false"})
    try:
        for label, init_js, want in (("upright", None, (LEFT_HALF, RIGHT_HALF)),
                                     ("half turn", HALF_TURN_JS, (RIGHT_HALF, LEFT_HALF))):
            with sync_playwright() as p:
                browser, page, errors = launch(p, "chromium", y4m, "websockets", init_js=init_js)
                res.check(f"{label}: stream up", bool(C.wait_ws_video(page)))
                toggle(page, True)
                res.check(f"{label}: webcam reports active", wait_status(page, True))
                deadline = time.time() + 20
                while time.time() < deadline and probe(2, timeout_ms=1500).get("rc") != 0:
                    time.sleep(0.5)
                r = probe(30, samples=[left_at, right_at])
                res.check(f"{label}: frames reach /dev/video0", r.get("rc") == 0, str(r.get("error", "")))
                res.check(f"{label}: left of the device picture", near(r["samples"].get(left_at), want[0]),
                          f"{r['samples'].get(left_at)} want {want[0]}")
                res.check(f"{label}: right of the device picture", near(r["samples"].get(right_at), want[1]),
                          f"{r['samples'].get(right_at)} want {want[1]}")
                res.check(f"{label}: no page errors", not errors, "; ".join(errors)[:200])
                toggle(page, False)
                wait_status(page, False)
                browser.close()
                time.sleep(1.5)
    finally:
        H.server_stop()
    return res


def reformat_block() -> "H.Results":
    """The ``auto`` device follows the uplink that is actually there.

    The camera outlives the session that started it, so an uplink of the other kind finds a
    device it does not fit: a video uplink in an MJPEG device is decoded and re-encoded per
    frame, an MJPEG uplink in a raw device decoded where it could have passed through. It is
    re-created for the newcomer -- but only while nothing reads it, which an attached
    interposer client proves by keeping the device exactly as it was.
    """
    res = H.Results("webcam-reformat")
    y4m = os.path.join(tempfile.mkdtemp(prefix="selkies-cam-"), "green.y4m")
    green_y4m(y4m)
    H.server_start(mode="websockets", wayland=False, extra_env={"SELKIES_WEBCAM_ENABLED": "false"})
    reader = None
    try:
        with sync_playwright() as p:
            browser, page, _errors = launch(p, "chromium", y4m, "websockets")
            C.wait_ws_video(page, timeout=30)
            toggle(page, True)
            wait_status(page, True)
            res.check("a video uplink brings the device up raw", wait_format("YU12"), device_format())
            browser.close()

        # An interposer client that outlives the switch: the device it opened must not change
        # under it, whatever the next uplink would prefer.
        reader = start_reader()
        with sync_playwright() as p:
            browser, page, _errors = launch(p, "chromium", y4m, "websockets", init_js=NO_WEBCODECS_JS)
            C.wait_ws_video(page, timeout=30)
            toggle(page, True)
            wait_status(page, True)
            time.sleep(REFORMAT_SETTLE)
            res.check("a JPEG uplink does not re-create a device being read",
                      device_format() == "YU12", device_format())
            reader.terminate()
            reader.wait(timeout=10)
            reader = None
            res.check("and re-creates it once the reader is gone", wait_format("MJPG", timeout=30),
                      device_format())
            toggle(page, False)
            wait_status(page, False)
            browser.close()
    finally:
        if reader is not None:
            reader.terminate()
            reader.wait(timeout=10)
        H.server_stop()
    return res


def main() -> int:
    sel = sys.argv[1] if len(sys.argv) > 1 else "websockets"
    build()
    if sel == "rotation":
        ok = rotation_block().summary()
    elif sel == "locked":
        ok = locked_block().summary()
    elif sel == "nowebcodecs":
        ok = nowebcodecs_block().summary()
    elif sel == "reformat":
        ok = reformat_block().summary()
    else:
        ok = transport_block(sel).summary()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
