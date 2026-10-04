#!/usr/bin/env python3
"""The encoders past the defaults, decoded by a real browser on every cell.

striped:  a server configured with ``h264enc-striped``. Over WebSockets the
          stream arrives as several independent H.264 stripes per frame (read
          off the 0x04 frame headers on the wire), encoded in software, and
          the picture decodes. WebRTC carries no striped framing, so there the
          configured encoder is refused with a log line and the stream comes
          up on h264enc.
cpu:      ``h264enc`` forced onto the software encoder (``use_cpu``): the
          stream is a single full-frame stripe, the log names the CPU encoder,
          and the picture decodes.
switch:   over WebSockets the classic dashboard's encoder select moves a live
          session from the default encoder onto h264enc-striped and back; the
          server restarts the capture each way and the picture survives.
          Over WebRTC the same select must not offer the striped encoder.

The picture is two known colors the test paints on the server: X11 windows on
the test display, or the Wayland observer surface filled with one and carrying
the other, sampled from the decoded frame in the page. Over WebSockets the page
runs in Chromium; over WebRTC each block runs in Chromium, Firefox and WebKit,
whose own RTP receivers decode and paint the stream, and the <video> must keep
presenting frames while the screen changes.

    python3 tests/e2e/test_encoders.py ws-x11|wr-x11|ws-wl|wr-wl
"""
import os
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_dashboards as TD
from playwright.sync_api import sync_playwright

WL_SOCKET = "wayland-1"
# The color painted on the server, and how far a decoded sample may stray
# from it (4:2:0 chroma, limited range, and two codecs' rounding).
PAINT = (40, 120, 220)
PAINT_ARGB = "ff2878dc"
TOLERANCE = 24
# A saturated second block guards the color matrix: a stream converted with one
# matrix and painted with another lands more than twenty levels off here, where
# the first block barely moves. Red rather than green, because
# libyuv -- which Chromium and Firefox both convert through -- clamps the
# BT.709 Cb-to-blue coefficient to 2.0 from 2.112, which costs a green block
# eleven levels of blue and this one two.
SATURATED = (255, 0, 0)
SATURATED_ARGB = "ffff0000"
SATURATED_TOLERANCE = 10
# Where the blocks sit, and a spot well outside them.
BLOCK = (100, 100, 300, 200)
BLOCK2 = (500, 100, 300, 200)
INSIDE, OUTSIDE, INSIDE2 = (250, 200), (900, 600), (650, 200)

# Every 0x04 frame on the WebSocket carries its stripe's Y start and height in
# the header; a striped stream has several starts, a full-frame one only 0.
# Undiverted frames are read off the worker-socket handle (and a plain page
# socket, should the worker fall back); frames diverted straight to the video
# worker never surface here and are reported by window.videoStripeRows instead.
STRIPE_TAP = """
(() => {
  window.__stripes = {};
  const tap = (e) => {
    if (!(e.data instanceof ArrayBuffer) || e.data.byteLength < 10) return;
    const v = new DataView(e.data);
    if (v.getUint8(0) !== 0x04) return;
    window.__stripes[v.getUint16(4, false)] = v.getUint16(8, false);
  };
  const WS = window.WebSocket;
  window.WebSocket = function(...a) {
    const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
    s.addEventListener('message', tap);
    return s;
  };
  window.WebSocket.prototype = WS.prototype;
  Object.setPrototypeOf(window.WebSocket, WS);
  let transport = null;
  Object.defineProperty(window, 'selkiesTransport', {
    configurable: true,
    get: () => transport,
    set: (v) => {
      transport = v;
      if (v && v.addEventListener && !v.__stripeTapped) {
        v.__stripeTapped = true;
        v.addEventListener('message', tap);
      }
    },
  });
})();
"""

# The decoded picture: whichever sink is showing (the <video> a full-frame
# mode renders through, else the canvas), drawn once into a scratch canvas.
SAMPLE_JS = """
([ix, iy, ox, oy, sx, sy]) => {
  const v = document.querySelector('video');
  let src = null, w = 0, h = 0, kind = '';
  if (v && v.videoWidth > 0 && v.readyState >= 2 && v.style.display !== 'none') {
    src = v; w = v.videoWidth; h = v.videoHeight; kind = 'video';
  } else {
    for (const c of document.querySelectorAll('canvas')) {
      if (c.width >= 640 && c.style.display !== 'none') { src = c; w = c.width; h = c.height; kind = 'canvas'; break; }
    }
  }
  if (!src) return null;
  const oc = document.createElement('canvas'); oc.width = w; oc.height = h;
  const ctx = oc.getContext('2d'); ctx.drawImage(src, 0, 0, w, h);
  const d = ctx.getImageData(0, 0, w, h).data;
  // A point past a picture still smaller than the page (a start or a resize) reads as no sample.
  const px = (x, y) => {
    if (x >= w || y >= h) return null;
    const i = (y * w + x) * 4; return [d[i], d[i + 1], d[i + 2]];
  };
  return {kind, w, h, inside: px(ix, iy), outside: px(ox, oy), saturated: px(sx, sy)};
}
"""

# Frames the <video> presents, counted from the first call through
# requestVideoFrameCallback, which every engine has (a playback-quality count
# of a live stream is not kept everywhere).
PRESENTED_JS = """() => {
  const v = document.querySelector('video');
  if (!v || !v.requestVideoFrameCallback) return null;
  if (v.__presented === undefined) {
    v.__presented = 0;
    const tick = () => { v.__presented++; v.requestVideoFrameCallback(tick); };
    v.requestVideoFrameCallback(tick);
  }
  return v.__presented;
}"""
# The changing screen they are counted on for LIVE_SECS, LIVE_MIN_FRAMES at
# least: on X11 a small window clear of the sampled points flips between two
# grays every LIVE_FLIP_S; the Wayland surface blinks as often to a second fill
# within the picture's tolerance.
LIVE_SECS = 2.0
LIVE_FLIP_S = 0.05
LIVE_MIN_FRAMES = 10
FLICKER = (1000, 400, 120, 80)
FLICKER_GRAYS = (0x404040, 0xC0C0C0)
PAINT2_ARGB = "ff2c7cd8"


def near(rgb: Optional[list], want: tuple, tolerance: int = TOLERANCE) -> bool:
    return rgb is not None and all(abs(a - b) <= tolerance for a, b in zip(rgb, want))


def paint_x11() -> Any:
    """Map the two solid override-redirect windows on the test display; closing
    the returned display connection takes them down again."""
    from selkies.Xlib import display as xdisp, X
    d = xdisp.Display(H.require_display())
    scr = d.screen()
    for block, argb in ((BLOCK, PAINT_ARGB), (BLOCK2, SATURATED_ARGB)):
        win = scr.root.create_window(*block, 0, scr.root_depth, window_class=X.InputOutput,
                                     background_pixel=int(argb[2:], 16), override_redirect=True)
        win.map()
    d.sync()
    return d


class Picture:
    """The painted picture as the page decodes it: the X11 blocks sit in a
    black frame, the Wayland observer surface covers the frame in the first
    color and carries the saturated block."""

    def __init__(self, wayland: bool, live: bool = False) -> None:
        self.wayland = wayland
        self.live = live
        self.handle = None

    def paint(self) -> None:
        if self.wayland:
            os.environ["WLOBS_FILL"] = PAINT_ARGB
            os.environ["WLOBS_BLOCK"] = ",".join(map(str, BLOCK2)) + "," + SATURATED_ARGB
            blink = {"WLOBS_FILL2": PAINT2_ARGB, "WLOBS_BLINK_MS": str(int(LIVE_FLIP_S * 1000))}
            self.handle = H.WlObs(WL_SOCKET, **(blink if self.live else {}))
            self.handle.ready(20)
        else:
            self.handle = paint_x11()

    def presenting(self, page: Any) -> dict:
        """The frames the page presents while the screen changes for LIVE_SECS
        (a `live` picture on Wayland blinks on its own)."""
        start = page.evaluate(PRESENTED_JS)
        flips = 0
        if self.wayland:
            time.sleep(LIVE_SECS)
            flips = round(LIVE_SECS / LIVE_FLIP_S)
        else:
            from selkies.Xlib import X
            d = self.handle
            scr = d.screen()
            win = scr.root.create_window(*FLICKER, 0, scr.root_depth, window_class=X.InputOutput,
                                         background_pixel=FLICKER_GRAYS[0], override_redirect=True)
            win.map()
            d.sync()
            end = time.time() + LIVE_SECS
            while time.time() < end:
                flips += 1
                win.change_attributes(background_pixel=FLICKER_GRAYS[flips % 2])
                win.clear_area()
                d.sync()
                time.sleep(LIVE_FLIP_S)
            win.destroy()
            d.sync()
        frames = page.evaluate(PRESENTED_JS)
        return {"frames": None if frames is None else frames - (start or 0), "secs": LIVE_SECS, "flips": flips}

    def keeps_presenting(self, res: "H.Results", tag: str, page: Any, waived: str = "") -> None:
        name = f"{tag}: frames keep presenting on a changing screen"
        if waived:
            res.skip(name, waived)
            return
        live = self.presenting(page)
        res.check(name, (live["frames"] or 0) >= LIVE_MIN_FRAMES, live)

    def clear(self) -> None:
        if self.handle is None:
            return
        if self.wayland:
            os.environ.pop("WLOBS_FILL", None)
            os.environ.pop("WLOBS_BLOCK", None)
            self.handle.stop()
        else:
            self.handle.close()
        self.handle = None

    @staticmethod
    def sample_page(page: Any) -> Optional[dict]:
        return page.evaluate(SAMPLE_JS, [*INSIDE, *OUTSIDE, *INSIDE2])

    def sample(self, page: Any) -> Optional[dict]:
        return Picture.sample_page(page)

    def matches(self, sample: Optional[dict], matrix: bool = True) -> bool:
        """Whether the sample shows the painted picture; `matrix` also holds the
        saturated block to its tolerance, which an engine painting the stream
        with a matrix the codec cannot declare to it fails through no fault of
        the stream."""
        if not sample:
            return False
        ground = PAINT if self.wayland else (0, 0, 0)
        return (near(sample["inside"], PAINT) and near(sample["outside"], ground)
                and (not matrix or near(sample["saturated"], SATURATED, SATURATED_TOLERANCE)))

    def wait(self, page: Any, timeout: float = 20, matrix: bool = True) -> Optional[dict]:
        deadline = time.time() + timeout
        sample = None
        while time.time() < deadline:
            sample = self.sample(page)
            if self.matches(sample, matrix):
                return sample
            time.sleep(0.5)
        return sample


def open_page(p: Any, mode: str, engine: str = "chromium") -> tuple:
    """A page of `engine` on the core client in `mode`, with its browser or context to close."""
    if engine == "firefox":
        ctx = C.firefox_persistent_context(p, viewport={"width": 1280, "height": 720})
        owner = ctx
    else:
        owner = C.launch_browser(p, engine)
        ctx = owner.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    # Firefox runs on one persistent profile: the encoder a previous block's
    # ladder stored must not become this block's pick.
    ctx.add_init_script("try { localStorage.clear(); } catch (e) {}")
    ctx.add_init_script(STRIPE_TAP)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    return owner, page


def wait_video(page: Any, mode: str) -> Optional[dict]:
    return C.wait_ws_video(page, timeout=30) if mode == "websockets" else C.wait_wr_video(page)


def wait_divert(page: Any, want: bool, timeout: float = 15, presented: bool = False) -> dict:
    """Poll the divert state the core publishes until it matches, or time out.

    ``presented`` also waits for a frame to have been presented. The row layout
    is published as the first stripes are decoded, while the frame rate is
    counted over a window that closes later, so a caller asserting one has to
    wait for it rather than for the rows that arrive before it.
    """
    deadline = time.time() + timeout
    state = {}
    while time.time() < deadline:
        state = page.evaluate("""({
          on: !!window.videoDivertOn,
          rows: Object.keys(window.videoStripeRows || {}).length,
          fps: window.fps || 0,
        })""")
        settled = state["rows"] > 0 and (not presented or state["fps"] > 0)
        if state["on"] == want and (not want or settled):
            return state
        time.sleep(0.5)
    return state


def stripes(page: Any) -> dict:
    """Stripe Y start -> height seen on the wire so far: the page tap merged
    with the row layout the video worker reports while the divert holds."""
    seen = page.evaluate(
        "Object.assign({}, window.videoStripeRows || {}, window.__stripes)")
    return {int(k): v for k, v in seen.items()}


def wait_stripes(page: Any, striped: bool, timeout: float = 15) -> dict:
    """Poll the wire until it shows the striping asked for, or time runs out."""
    deadline = time.time() + timeout
    seen = {}
    while time.time() < deadline:
        page.evaluate("window.__stripes = {}; window.videoStripeRows = {};")
        time.sleep(1.2)
        seen = stripes(page)
        if seen and (len(seen) > 1) == striped:
            return seen
    return seen


def is_striped(seen: dict, frame_h: int) -> bool:
    return len(seen) > 1 and max(seen.values()) < frame_h


def is_fullframe(seen: dict, frame_h: int) -> bool:
    return list(seen) == [0] and seen[0] == frame_h


def last_stream_line(log_from: int = 0) -> str:
    """The newest 'Stream settings active' line pixelflux printed after `log_from`."""
    txt = H.server_log()[log_from:]
    lines = [line for line in txt.splitlines() if "Stream settings active" in line]
    return lines[-1] if lines else ""


def wait_stream_line(count: int, timeout: float = 20) -> str:
    """The stream line once pixelflux has printed more than `count` of them."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        txt = H.server_log()
        if txt.count("Stream settings active") > count:
            return last_stream_line()
        time.sleep(0.5)
    return ""


def encoder_field(line: str) -> str:
    """The encoder part of a stream line, for a check's detail."""
    at = max(line.find("Encoder:"), line.find("Mode:"))
    return line[at:at + 60] if at >= 0 else line[-80:]


def cpu_encoder(line: str) -> bool:
    """Whether a stream line names the software H.264 encoder: pixelflux prints
    it as CPU on X11 and by its library name on Wayland, never as a GPU."""
    software = any(name in line for name in ("CPU", "x264", "OpenH264", "openh264"))
    return software and "NVENC" not in line and "VAAPI" not in line


def block_striped(mode: str, wayland: bool, res: "H.Results", engine: str = "chromium") -> None:
    tag = "striped" if engine == "chromium" else "striped " + engine
    H.server_start(mode=mode, wayland=wayland, extra_env={"SELKIES_ENCODER": "h264enc-striped"})
    picture = Picture(wayland, live=mode == "webrtc")
    try:
        picture.paint()
        with sync_playwright() as p:
            owner, page = open_page(p, mode, engine)
            try:
                video = wait_video(page, mode)
                res.check(f"{tag}: stream up", bool(video), video)
                line = wait_stream_line(0)
                if mode == "websockets":
                    enc = page.evaluate("window.encoder")
                    res.check(f"{tag}: client follows the server's encoder", enc == "h264enc-striped", enc)
                    seen = wait_stripes(page, striped=True)
                    res.check(f"{tag}: several H.264 stripes per frame on the wire",
                              video and is_striped(seen, video["h"]), seen)
                    res.check(f"{tag}: encoded in software", cpu_encoder(line), encoder_field(line))
                    divert = wait_divert(page, True, presented=True)
                    res.check(f"{tag}: the video worker decodes and presents it",
                              divert["on"] and divert["rows"] > 1 and divert["fps"] > 0, divert)
                    page.evaluate("window.__stripes = {}")
                    time.sleep(1.5)
                    leaked = page.evaluate("Object.keys(window.__stripes).length")
                    res.check(f"{tag}: no stripe reaches the page while diverted", leaked == 0, leaked)
                    vpage = C.new_page(page.context, url_hash="#shared")
                    time.sleep(6.0)
                    vinfo = C.wait_ws_video(vpage, timeout=20)
                    res.check(f"{tag}: a shared viewer gets the stream", vinfo is not None, vinfo)
                    vsample = Picture.sample_page(vpage)
                    res.check(f"{tag}: the shared picture decodes at inferred geometry",
                              vsample is not None and near(vsample["inside"], PAINT), vsample)
                    vpage.close()
                else:
                    res.check(f"{tag}: refused for WebRTC, h264enc used instead",
                              C.wait_log("not available for WebRTC", timeout=5)
                              and C.wait_log("using 'h264enc'", timeout=5), "")
                sample = picture.wait(page)
                res.check(f"{tag}: the painted picture decodes", picture.matches(sample), sample)
                if mode == "webrtc":
                    picture.keeps_presenting(res, tag, page)
            finally:
                owner.close()
    finally:
        picture.clear()
        H.server_stop()


def block_cpu(mode: str, wayland: bool, res: "H.Results", engine: str = "chromium") -> None:
    tag = "cpu" if engine == "chromium" else "cpu " + engine
    H.server_start(mode=mode, wayland=wayland,
                   extra_env={"SELKIES_ENCODER": "h264enc", "SELKIES_USE_CPU": "true"})
    picture = Picture(wayland, live=mode == "webrtc")
    try:
        picture.paint()
        with sync_playwright() as p:
            owner, page = open_page(p, mode, engine)
            try:
                video = wait_video(page, mode)
                res.check(f"{tag}: stream up", bool(video), video)
                line = wait_stream_line(0)
                res.check(f"{tag}: software encoder in use", cpu_encoder(line), encoder_field(line))
                if mode == "websockets":
                    seen = wait_stripes(page, striped=False)
                    res.check(f"{tag}: one full-frame stripe on the wire",
                              video and is_fullframe(seen, video["h"]), seen)
                sample = picture.wait(page)
                res.check(f"{tag}: the painted picture decodes", picture.matches(sample), sample)
                if mode == "webrtc":
                    picture.keeps_presenting(res, tag, page)
            finally:
                owner.close()
    finally:
        picture.clear()
        H.server_stop()


def block_switch(mode: str, wayland: bool, res: "H.Results", engine: str = "chromium") -> None:
    tag = "switch" if engine == "chromium" else "switch " + engine
    H.server_start(mode=mode, wayland=wayland, web_root=H.CLASSIC_DIST)
    picture = Picture(wayland)
    try:
        picture.paint()
        with sync_playwright() as p:
            owner, page = open_page(p, mode, engine)
            try:
                video = wait_video(page, mode)
                res.check(f"{tag}: stream up", bool(video), video)
                opened = TD.classic_open_video(page)
                options = page.evaluate(
                    "Array.from(document.querySelectorAll('#encoderSelect option')).map(o => o.value)") if opened else []
                if mode == "webrtc":
                    # WebRTC carries the full-frame encoders alone, those the
                    # engine's RTP receiver takes; the striped framings never show.
                    full_frame = ("h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc")
                    res.check(f"{tag}: the dashboard offers WebRTC only full-frame encoders",
                              opened and "h264enc" in options
                              and all(o in full_frame for o in options), (opened, options))
                    return
                res.check(f"{tag}: the dashboard offers the striped encoder", "h264enc-striped" in options, options)
                seen = wait_stripes(page, striped=False)
                res.check(f"{tag}: default stream is full-frame", video and is_fullframe(seen, video["h"]), seen)
                before = H.server_log().count("Stream settings active")
                page.select_option("#encoderSelect", "h264enc-striped")
                line = wait_stream_line(before)
                res.check(f"{tag}: capture restarted on the striped software encoder", cpu_encoder(line), encoder_field(line))
                seen = wait_stripes(page, striped=True)
                res.check(f"{tag}: stripes on the wire after the switch", video and is_striped(seen, video["h"]), seen)
                sample = picture.wait(page)
                res.check(f"{tag}: the picture decodes striped", picture.matches(sample), sample)
                divert = wait_divert(page, True)
                res.check(f"{tag}: the striped stream diverts to the video worker",
                          divert["on"] and divert["rows"] > 1, divert)
                before = H.server_log().count("Stream settings active")
                page.select_option("#encoderSelect", "jpeg")
                line = wait_stream_line(before)
                res.check(f"{tag}: capture restarted on jpeg", bool(line), encoder_field(line))
                seen = wait_stripes(page, striped=True)
                res.check(f"{tag}: jpeg stripes on the wire", video and is_striped(seen, video["h"]), seen)
                divert = wait_divert(page, True)
                res.check(f"{tag}: jpeg decodes in the video worker",
                          divert["on"] and divert["rows"] > 1, divert)
                sample = picture.wait(page)
                res.check(f"{tag}: the picture decodes as jpeg", picture.matches(sample), sample)
                before = H.server_log().count("Stream settings active")
                page.select_option("#encoderSelect", "h264enc")
                line = wait_stream_line(before)
                res.check(f"{tag}: capture restarted back on h264enc", bool(line), encoder_field(line))
                seen = wait_stripes(page, striped=False)
                res.check(f"{tag}: full-frame again on the wire", video and is_fullframe(seen, video["h"]), seen)
                sample = picture.wait(page)
                res.check(f"{tag}: the picture decodes full-frame again", picture.matches(sample), sample)
            finally:
                owner.close()
    finally:
        picture.clear()
        H.server_stop()


SELECTORS = ("ws-x11", "wr-x11", "ws-wl", "wr-wl")
ENGINES = ("chromium", "firefox", "webkit")


def main() -> bool:
    which = sys.argv[1] if len(sys.argv) > 1 else "ws-x11"
    if which not in SELECTORS:
        raise SystemExit(f"unknown selector {which!r}; one of {SELECTORS}")
    transport, backend = which.split("-")
    mode = "websockets" if transport == "ws" else "webrtc"
    wayland = backend == "wl"
    res = H.Results(f"encoders-{which}")
    for engine in ENGINES if mode == "webrtc" else ENGINES[:1]:
        for block in (block_striped, block_cpu, block_switch):
            block(mode, wayland, res, engine)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
