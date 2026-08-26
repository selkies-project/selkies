#!/usr/bin/env python3
"""The webcam uplink end to end: a browser's camera, switched on through the
dashboard's pipeline control, must come out of the virtual V4L2 device on the
server — over the WebSocket (WebCodecs frames) and over WebRTC (the camera
track on the reserved sendonly transceiver) — and switch off again.

Both engines capture the same camera: pixelflux's ``VirtualCamera`` published
through the interposer, so the device's pixels are checked rather than just
their presence and neither engine is measured on content the other never sees.
Which codec an engine settles on is left to what it measured it could sustain
and is never asserted -- that moves with the browser -- while the camera
reaching the device at its own rate has to hold whatever was chosen.
The operator lock (``webcam_enabled=false|locked``) must withhold the uplink
on both transports.

    python3 tests/e2e/test_webcam.py websockets|webrtc|locked
"""
import io
import os
import random
import subprocess
import sys
import tempfile
import threading
import time
from typing import Optional

import pixelflux
from PIL import Image, ImageChops, ImageDraw

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


# Camera content is authored in YCbCr, which is what the device carries and what
# the samples below are checked against; a JPEG round trip leaves a flat colour
# where it started, so the published pixels and the device's are the same ones.
GREEN = (150, 44, 21)
BLACK = (16, 128, 128)

# Two flat halves in one frame: a turn relayed on the wire has to swap them in
# the device picture, which a solid frame could never show.
LEFT_HALF = (150, 44, 21)
RIGHT_HALF = (80, 200, 120)

# The rate the camera publishes at, and the share of it the device must carry.
# Whichever rung the client's ladder settled on has to sustain the camera; one
# that cannot is the failure this guards, and detailed content -- where the
# choice is a real one -- is held to more.
CAMERA_FPS = 30
RATE_FLOOR = 0.4
DETAIL_RATE_FLOOR = 0.6


def encode(im: "Image.Image") -> bytes:
    buf = io.BytesIO()
    im.save(buf, "JPEG", quality=92, subsampling=0)
    return buf.getvalue()


def flat_frames(colour=GREEN, width: int = 640, height: int = 480):
    return [encode(Image.new("YCbCr", (width, height), colour))]


def split_frames(width: int = 640, height: int = 480):
    """A frame split down the middle, for the rotation checks."""
    im = Image.new("YCbCr", (width, height), LEFT_HALF)
    ImageDraw.Draw(im).rectangle([width // 2, 0, width, height], fill=RIGHT_HALF)
    return [encode(im)]


def detail_frames(width: int = 1280, height: int = 720, count: int = 24):
    """A detailed scene that pans under moving grain: what an HD lens costs an
    encoder. Flat or repeating content is encoded almost for free and would rank
    every codec as fast enough, which is the measurement error this exists to
    keep out of the codec decision."""
    rnd = random.Random(7)
    scene = Image.new("YCbCr", (width * 2, height), (90, 128, 128))
    draw = ImageDraw.Draw(scene)
    for _ in range(900):
        x, y = rnd.randrange(scene.width), rnd.randrange(height)
        w, h = rnd.randrange(6, 80), rnd.randrange(6, 80)
        draw.rectangle([x, y, x + w, y + h],
                       fill=(rnd.randrange(256), rnd.randrange(256), rnd.randrange(256)))
    grain = Image.frombytes("L", (width * 2, height), os.urandom(width * 2 * height))
    out = []
    for i in range(count):
        off = int(i * width / count)
        frame = scene.crop((off, 0, off + width, height))
        speck = grain.crop((width - off, 0, 2 * width - off, height)).point(lambda v: v // 8)
        luma, cb, cr = frame.split()
        out.append(encode(Image.merge("YCbCr", (ImageChops.add(luma, speck), cb, cr))))
    return out


class PublishedCamera:
    """The camera both engines capture. pixelflux's VirtualCamera publishes the
    frames through the interposer, so Firefox and Chromium read the same pixels
    at the same rate rather than each engine's own synthetic fake, and what the
    camera is showing is the test's to choose."""

    def __init__(self, frames, width: int = 640, height: int = 480, fps: int = CAMERA_FPS,
                 pixel_format: str = "I420"):
        self.frames, self.width, self.height, self.fps = frames, width, height, fps
        self.pixel_format = pixel_format
        self.sock_dir = tempfile.mkdtemp(prefix="selkies-cam-")
        self._cam = None
        self._stop = threading.Event()

    def start(self) -> "PublishedCamera":
        settings = pixelflux.VirtualCameraSettings()
        settings.socket_path = os.path.join(self.sock_dir, "selkies_webcam0.sock")
        settings.width, settings.height = self.width, self.height
        settings.pixel_format = self.pixel_format
        settings.device_path = ""
        # Reachable only over its own socket, which is the browser's to read: a
        # PipeWire node would also be found by the probe and the device under
        # test would then be this camera rather than the server's.
        settings.pipewire = False
        self._cam = pixelflux.VirtualCamera()
        self._cam.start(settings)
        threading.Thread(target=self._pump, daemon=True).start()
        time.sleep(0.5)
        return self

    def _pump(self) -> None:
        i = 0
        while not self._stop.is_set():
            try:
                self._cam.push(self.frames[i % len(self.frames)],
                               pixelflux.VirtualCamera.CODEC_MJPEG, True)
            except Exception:
                return
            i += 1
            time.sleep(1.0 / self.fps)

    def stop(self) -> None:
        self._stop.set()
        try:
            self._cam.stop()
        except Exception:
            pass


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


# The picture reaches the device through whichever codec the engine chose, so a
# flat colour arrives a few steps off and how many depends on that choice. The
# colours checked against each other are tens of steps apart, so a tolerance
# wide enough for any of those round trips still tells them apart.
COLOUR_TOL = 20


def near(got, want, tol=COLOUR_TOL) -> bool:
    return got is not None and all(abs(g - w) <= tol for g, w in zip(got, want))


def wait_for_picture(wants, frames: int = 30, timeout: float = 30) -> dict:
    """Poll until the device shows this uplink's picture, then measure it.

    A device that exists is not yet a device carrying the camera: the server
    decodes what arrives, and the picture lands a moment after the first frame
    does -- sooner or later depending on the engine and the rung it took. Waiting
    on the picture rather than on a fixed delay keeps that out of the checks.
    """
    samples = [point for point, _ in wants]
    deadline = time.time() + timeout
    while time.time() < deadline:
        r = probe(5, timeout_ms=3000, samples=samples)
        # Every sample, not just one: a frame caught mid-transition is a single
        # flat colour, which one sample on its own cannot tell from the picture.
        if r.get("rc") == 0 and all(near(r["samples"].get(point), want) for point, want in wants):
            break
        time.sleep(0.5)
    return probe(frames, samples=samples)


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


def launch(p, engine: str, cam_sock: str, mode: str, init_js: Optional[str] = None):
    """Open the dashboard in `engine` with the published camera as its only device.

    Both engines capture that camera through the interposer rather than an
    engine-specific fake, so what they are asked to encode is the same.

    Returns:
        `(browser, page, errors)`, where `errors` collects page errors as they occur.
    """
    env = dict(os.environ, LD_PRELOAD=INTERPOSER, SELKIES_WEBCAM_SOCKET_PATH=cam_sock)
    if engine == "firefox":
        prefs = {
            "media.navigator.permission.disabled": True,
            # Stated, not omitted: the persistent profile carries whatever a
            # previous run left behind, and a fake camera persisted there would
            # be captured instead of the published one.
            "media.navigator.streams.fake": False,
            "media.autoplay.default": 0,
            "media.autoplay.blocking_policy": 0,
            "media.autoplay.block-webaudio": False,
            "media.gmp-gmpopenh264.enabled": True,
            **TB.openh264_prefs(),
        }
        if TB.openh264_version():
            # The side-loaded OpenH264 lives in the persistent e2e profile.
            ctx = p.firefox.launch_persistent_context(user_data_dir=TB.FF_E2E_PROFILE, headless=True,
                                                      viewport={"width": 1280, "height": 720},
                                                      firefox_user_prefs=prefs, env=env)
            browser = ctx.browser or ctx
        else:
            kw = {"headless": True, "firefox_user_prefs": prefs, "env": env}
            if C.FIREFOX_PATH:
                kw["executable_path"] = C.FIREFOX_PATH
            browser = p.firefox.launch(**kw)
            ctx = browser.new_context(viewport={"width": 1280, "height": 720})
    else:
        # The headless shell has no media capture; the full Chromium build (new
        # headless mode) or the system Chrome named by E2E_CHROME is needed.
        args = C.BROWSER_ARGS + ["--use-fake-ui-for-media-stream"]
        kw = {"headless": True, "args": args, "env": env}
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
    cam = PublishedCamera(flat_frames()).start()
    dump = os.path.join(cam.sock_dir, "frame.jpg")
    H.server_start(mode="websockets", wayland=False, extra_env={"SELKIES_WEBCAM_ENABLED": "false"})
    try:
        with sync_playwright() as p:
            browser, page, errors = launch(p, "chromium", cam.sock_dir, "websockets", init_js=NO_WEBCODECS_JS)
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
        cam.stop()
    return res


def transport_block(mode: str) -> "H.Results":
    """The camera comes out of /dev/video0 over `mode` from both engines and stops
    with the uplink.

    The device is pinned to I420 whatever the client sends: by default its format
    follows the uplink, which would make the picture readable only on some rungs,
    and which rung an engine takes is not this block's to fix -- the reformat
    selector exercises that default. The process-wide camera outlives whichever
    uplink brought it up and is decoded into the same device on either transport.
    """
    res = H.Results(f"webcam-{mode}")
    cam = PublishedCamera(flat_frames()).start()
    H.server_start(mode=mode, wayland=False,
                   extra_env={"SELKIES_WEBCAM_ENABLED": "false",
                              "SELKIES_WEBCAM_PIXEL_FORMAT": "I420"})
    try:
        for engine in ("chromium", "firefox"):
            with sync_playwright() as p:
                browser, page, errors = launch(p, engine, cam.sock_dir, mode)
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
                r = wait_for_picture([((640, 360), GREEN), ((20, 20), BLACK)])
                res.check(f"{engine}: 30 frames reach /dev/video0", r.get("rc") == 0 and r.get("frames") == "30",
                          f"rc={r.get('rc')} frames={r.get('frames')} err={r.get('error', '')}")
                res.check(f"{engine}: device is the configured 1280x720 I420",
                          (r.get("format"), r.get("width"), r.get("height")) == ("YU12", "1280", "720"),
                          f"{r.get('format')} {r.get('width')}x{r.get('height')}")
                res.check(f"{engine}: frames flow at the camera's rate",
                          float(r.get("fps", "0")) >= CAMERA_FPS * RATE_FLOOR,
                          f"{r.get('fps')} of {CAMERA_FPS}")
                # The 4:3 picture sits pillarboxed in the 16:9 device: centre green,
                # the bar at x=20 black.
                res.check(f"{engine}: centre is the camera's green",
                          near(r["samples"].get((640, 360)), GREEN), str(r["samples"]))
                res.check(f"{engine}: pillarbox is black",
                          near(r["samples"].get((20, 20)), (16, 128, 128)), str(r["samples"]))
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
        cam.stop()
    return res


def locked_block() -> "H.Results":
    res = H.Results("webcam-locked")
    cam = PublishedCamera(flat_frames()).start()
    for mode in ("websockets", "webrtc"):
        H.server_start(mode=mode, wayland=False, extra_env={"SELKIES_WEBCAM_ENABLED": "false|locked"})
        try:
            with sync_playwright() as p:
                browser, page, _ = launch(p, "chromium", cam.sock_dir, mode)
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
    cam = PublishedCamera(split_frames()).start()
    # Well inside each half of the 4:3 picture as the server letterboxes it.
    left_at, right_at = (400, 360), (880, 360)
    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_WEBCAM_ENABLED": "false",
                              "SELKIES_WEBCAM_PIXEL_FORMAT": "I420"})
    try:
        for label, init_js, want in (("upright", None, (LEFT_HALF, RIGHT_HALF)),
                                     ("half turn", HALF_TURN_JS, (RIGHT_HALF, LEFT_HALF))):
            with sync_playwright() as p:
                browser, page, errors = launch(p, "chromium", cam.sock_dir, "websockets", init_js=init_js)
                res.check(f"{label}: stream up", bool(C.wait_ws_video(page)))
                toggle(page, True)
                res.check(f"{label}: webcam reports active", wait_status(page, True))
                r = wait_for_picture([(left_at, want[0]), (right_at, want[1])])
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
        cam.stop()
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
    cam = PublishedCamera(flat_frames()).start()
    H.server_start(mode="websockets", wayland=False, extra_env={"SELKIES_WEBCAM_ENABLED": "false"})
    reader = None
    try:
        with sync_playwright() as p:
            browser, page, _errors = launch(p, "chromium", cam.sock_dir, "websockets")
            C.wait_ws_video(page, timeout=30)
            toggle(page, True)
            wait_status(page, True)
            res.check("a video uplink brings the device up raw", wait_format("YU12"), device_format())
            browser.close()

        reader = start_reader()
        with sync_playwright() as p:
            browser, page, _errors = launch(p, "chromium", cam.sock_dir, "websockets", init_js=NO_WEBCODECS_JS)
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


def detail_block() -> "H.Results":
    """An HD camera showing real detail, which is where the encoder ladder makes
    a decision it can get wrong. Whichever rung an engine settles on has to carry
    the camera at close to its own rate: a codec the engine cannot sustain spends
    a core to deliver a fraction of the frames, and the device shows it. Nothing
    here asserts which rung was taken, so an engine whose codecs get faster keeps
    passing on the rung it earns."""
    res = H.Results("webcam-detail")
    cam = PublishedCamera(detail_frames(), width=1280, height=720).start()
    H.server_start(mode="websockets", wayland=False, extra_env={"SELKIES_WEBCAM_ENABLED": "false"})
    try:
        for engine in ("chromium", "firefox"):
            with sync_playwright() as p:
                browser, page, errors = launch(p, engine, cam.sock_dir, "websockets")
                res.check(f"{engine}: stream up", bool(C.wait_ws_video(page)), "")
                toggle(page, True)
                res.check(f"{engine}: webcam reports active", wait_status(page, True),
                          str(page.evaluate("window.__camStatus")))
                deadline = time.time() + 30
                while time.time() < deadline and probe(2, timeout_ms=1500).get("rc") != 0:
                    time.sleep(0.5)
                r = probe(60, timeout_ms=8000)
                res.check(f"{engine}: the detailed camera reaches the device",
                          r.get("rc") == 0 and r.get("frames") == "60",
                          f"rc={r.get('rc')} frames={r.get('frames')} err={r.get('error', '')}")
                res.check(f"{engine}: the chosen rung sustains the camera",
                          float(r.get("fps", "0")) >= CAMERA_FPS * DETAIL_RATE_FLOOR,
                          f"{r.get('fps')} of {CAMERA_FPS}")
                res.check(f"{engine}: no page errors", not errors, "; ".join(errors)[:200])
                toggle(page, False)
                browser.close()
    finally:
        H.server_stop()
        cam.stop()
    return res


WIRE_CODEC_JS = """
  (() => {
    window.__codecs = {};
    const send = WebSocket.prototype.send;
    WebSocket.prototype.send = function (data) {
      if (data instanceof ArrayBuffer && data.byteLength > 3) {
        const b = new Uint8Array(data);
        if (b[0] === 0x06) window.__codecs[b[1]] = (window.__codecs[b[1]] || 0) + 1;
      }
      return send.apply(this, arguments);
    };
  })();
"""


def encoderpref_block() -> "H.Results":
    """The `webcam_encoder` setting, proven by the uplink's codec bytes on the
    engine whose rung it decides (Firefox, no MediaStreamTrackProcessor): the
    default keeps it on MJPEG, an explicit codec re-opens the ladder with that
    codec alone, and a forced codec never yields another codec's id -- though
    it may degrade to JPEG (Firefox H.264 emits nothing through its plugin's
    cold start). Every mode lands the same green on the device."""
    res = H.Results("webcam-encoderpref")
    cam = PublishedCamera(flat_frames()).start()
    for pref, want, need in (("auto", {0}, 0), ("vp8", {2}, 2), ("h264", {0, 1}, None)):
        env = {"SELKIES_WEBCAM_ENABLED": "false", "SELKIES_WEBCAM_PIXEL_FORMAT": "I420"}
        if pref != "auto":
            env["SELKIES_WEBCAM_ENCODER"] = pref
        H.server_start(mode="websockets", wayland=False, extra_env=env)
        try:
            with sync_playwright() as p:
                browser, page, errors = launch(p, "firefox", cam.sock_dir, "websockets", init_js=WIRE_CODEC_JS)
                res.check(f"{pref}: stream up", bool(C.wait_ws_video(page)), "")
                toggle(page, True)
                res.check(f"{pref}: webcam reports active", wait_status(page, True),
                          str(page.evaluate("window.__camStatus")))
                r = wait_for_picture([((640, 360), GREEN)])
                res.check(f"{pref}: device shows the camera's green",
                          near(r["samples"].get((640, 360)), GREEN), str(r.get("samples")))
                codecs = page.evaluate("window.__codecs") or {}
                got = {int(k) for k, v in codecs.items() if v > 5}
                res.check(f"{pref}: wire codecs within {sorted(want)}",
                          bool(got) and got <= want and (need is None or need in got), str(codecs))
                res.check(f"{pref}: no page errors", not errors, "; ".join(errors)[:200])
                browser.close()
        finally:
            H.server_stop()
    cam.stop()
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
    elif sel == "detail":
        ok = detail_block().summary()
    elif sel == "encoderpref":
        ok = encoderpref_block().summary()
    else:
        ok = transport_block(sel).summary()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
