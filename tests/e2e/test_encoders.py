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

The picture is a known colour the test paints on the server: an X11 window on
the test display, or the Wayland observer surface filled solid, sampled from
the decoded frame in the page.

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
# The colour painted on the server, and how far a decoded sample may stray
# from it (4:2:0 chroma, limited range and two codecs' rounding).
PAINT = (40, 120, 220)
PAINT_ARGB = "ff2878dc"
TOLERANCE = 24
# Where the X11 window sits, and a spot well outside it.
BLOCK = (100, 100, 300, 200)
INSIDE, OUTSIDE = (250, 200), (900, 600)

# Every 0x04 frame on the WebSocket carries its stripe's Y start and height in
# the header; a striped stream has several starts, a full-frame one only 0.
STRIPE_TAP = """
(() => {
  window.__stripes = {};
  const WS = window.WebSocket;
  window.WebSocket = function(...a) {
    const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
    s.addEventListener('message', (e) => {
      if (!(e.data instanceof ArrayBuffer) || e.data.byteLength < 10) return;
      const v = new DataView(e.data);
      if (v.getUint8(0) !== 0x04) return;
      window.__stripes[v.getUint16(4, false)] = v.getUint16(8, false);
    });
    return s;
  };
  window.WebSocket.prototype = WS.prototype;
  Object.setPrototypeOf(window.WebSocket, WS);
})();
"""

# The decoded picture: whichever sink is showing (the <video> a full-frame
# mode renders through, else the canvas), drawn once into a scratch canvas.
SAMPLE_JS = """
([ix, iy, ox, oy]) => {
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
  const px = (x, y) => { const i = (y * w + x) * 4; return [d[i], d[i + 1], d[i + 2]]; };
  return {kind, w, h, inside: px(ix, iy), outside: px(ox, oy)};
}
"""


def near(rgb: Optional[list], want: tuple) -> bool:
    return rgb is not None and all(abs(a - b) <= TOLERANCE for a, b in zip(rgb, want))


def paint_x11() -> Any:
    """Map a solid override-redirect window on the test display; closing the
    returned display connection takes it down again."""
    from selkies.Xlib import display as xdisp, X
    d = xdisp.Display(H.require_display())
    scr = d.screen()
    win = scr.root.create_window(*BLOCK, 0, scr.root_depth, window_class=X.InputOutput,
                                 background_pixel=int(PAINT_ARGB[2:], 16), override_redirect=True)
    win.map()
    d.sync()
    return d


class Picture:
    """The painted picture as the page decodes it: the X11 block sits in a
    black frame, the filled observer surface covers the whole Wayland frame."""

    def __init__(self, wayland: bool) -> None:
        self.wayland = wayland
        self.handle = None

    def paint(self) -> None:
        if self.wayland:
            os.environ["WLOBS_FILL"] = PAINT_ARGB
            self.handle = H.WlObs(WL_SOCKET)
            self.handle.ready(20)
        else:
            self.handle = paint_x11()

    def clear(self) -> None:
        if self.handle is None:
            return
        if self.wayland:
            os.environ.pop("WLOBS_FILL", None)
            self.handle.stop()
        else:
            self.handle.close()
        self.handle = None

    def sample(self, page: Any) -> Optional[dict]:
        return page.evaluate(SAMPLE_JS, [*INSIDE, *OUTSIDE])

    def matches(self, sample: Optional[dict]) -> bool:
        if not sample:
            return False
        if self.wayland:
            return near(sample["inside"], PAINT) and near(sample["outside"], PAINT)
        return near(sample["inside"], PAINT) and near(sample["outside"], (0, 0, 0))

    def wait(self, page: Any, timeout: float = 20) -> Optional[dict]:
        deadline = time.time() + timeout
        sample = None
        while time.time() < deadline:
            sample = self.sample(page)
            if self.matches(sample):
                return sample
            time.sleep(0.5)
        return sample


def open_page(p: Any, mode: str) -> Any:
    browser = C.chromium_launch(p)
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(STRIPE_TAP)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    return browser, page


def wait_video(page: Any, mode: str) -> Optional[dict]:
    return C.wait_ws_video(page, timeout=30) if mode == "websockets" else C.wait_wr_video(page)


def stripes(page: Any) -> dict:
    """Stripe Y start -> height seen on the wire so far."""
    return {int(k): v for k, v in page.evaluate("window.__stripes").items()}


def wait_stripes(page: Any, striped: bool, timeout: float = 15) -> dict:
    """Poll the wire until it shows the striping asked for, or time runs out."""
    deadline = time.time() + timeout
    seen = {}
    while time.time() < deadline:
        page.evaluate("window.__stripes = {}")
        time.sleep(1.0)
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


def block_striped(mode: str, wayland: bool, res: "H.Results") -> None:
    H.server_start(mode=mode, wayland=wayland, extra_env={"SELKIES_ENCODER": "h264enc-striped"})
    picture = Picture(wayland)
    try:
        picture.paint()
        with sync_playwright() as p:
            browser, page = open_page(p, mode)
            try:
                video = wait_video(page, mode)
                res.check("striped: stream up", bool(video), video)
                line = wait_stream_line(0)
                if mode == "websockets":
                    enc = page.evaluate("window.encoder")
                    res.check("striped: client follows the server's encoder", enc == "h264enc-striped", enc)
                    seen = wait_stripes(page, striped=True)
                    res.check("striped: several H.264 stripes per frame on the wire",
                              video and is_striped(seen, video["h"]), seen)
                    res.check("striped: encoded in software", cpu_encoder(line), encoder_field(line))
                else:
                    res.check("striped: refused for WebRTC, h264enc used instead",
                              C.wait_log("not available for WebRTC", timeout=5)
                              and C.wait_log("using 'h264enc'", timeout=5), "")
                sample = picture.wait(page)
                res.check("striped: the painted picture decodes", picture.matches(sample), sample)
            finally:
                browser.close()
    finally:
        picture.clear()
        H.server_stop()


def block_cpu(mode: str, wayland: bool, res: "H.Results") -> None:
    H.server_start(mode=mode, wayland=wayland,
                   extra_env={"SELKIES_ENCODER": "h264enc", "SELKIES_USE_CPU": "true"})
    picture = Picture(wayland)
    try:
        picture.paint()
        with sync_playwright() as p:
            browser, page = open_page(p, mode)
            try:
                video = wait_video(page, mode)
                res.check("cpu: stream up", bool(video), video)
                line = wait_stream_line(0)
                res.check("cpu: software encoder in use", cpu_encoder(line), encoder_field(line))
                if mode == "websockets":
                    seen = wait_stripes(page, striped=False)
                    res.check("cpu: one full-frame stripe on the wire",
                              video and is_fullframe(seen, video["h"]), seen)
                sample = picture.wait(page)
                res.check("cpu: the painted picture decodes", picture.matches(sample), sample)
            finally:
                browser.close()
    finally:
        picture.clear()
        H.server_stop()


def block_switch(mode: str, wayland: bool, res: "H.Results") -> None:
    H.server_start(mode=mode, wayland=wayland, web_root=H.CLASSIC_DIST)
    picture = Picture(wayland)
    try:
        picture.paint()
        with sync_playwright() as p:
            browser, page = open_page(p, mode)
            try:
                video = wait_video(page, mode)
                res.check("switch: stream up", bool(video), video)
                opened = TD.classic_open_video(page)
                options = page.evaluate(
                    "Array.from(document.querySelectorAll('#encoderSelect option')).map(o => o.value)") if opened else []
                if mode == "webrtc":
                    # The server offers WebRTC a single encoder, so the dashboard has
                    # no choice to render; a select it does render must not carry
                    # the striped framing.
                    res.check("switch: the dashboard offers WebRTC no striped encoder",
                              opened and "h264enc-striped" not in options
                              and (not options or options == ["h264enc"]), (opened, options))
                    return
                res.check("switch: the dashboard offers the striped encoder", "h264enc-striped" in options, options)
                seen = wait_stripes(page, striped=False)
                res.check("switch: default stream is full-frame", video and is_fullframe(seen, video["h"]), seen)
                before = H.server_log().count("Stream settings active")
                page.select_option("#encoderSelect", "h264enc-striped")
                line = wait_stream_line(before)
                res.check("switch: capture restarted on the striped software encoder", cpu_encoder(line), encoder_field(line))
                seen = wait_stripes(page, striped=True)
                res.check("switch: stripes on the wire after the switch", video and is_striped(seen, video["h"]), seen)
                sample = picture.wait(page)
                res.check("switch: the picture decodes striped", picture.matches(sample), sample)
                before = H.server_log().count("Stream settings active")
                page.select_option("#encoderSelect", "h264enc")
                line = wait_stream_line(before)
                res.check("switch: capture restarted back on h264enc", bool(line), encoder_field(line))
                seen = wait_stripes(page, striped=False)
                res.check("switch: full-frame again on the wire", video and is_fullframe(seen, video["h"]), seen)
                sample = picture.wait(page)
                res.check("switch: the picture decodes full-frame again", picture.matches(sample), sample)
            finally:
                browser.close()
    finally:
        picture.clear()
        H.server_stop()


SELECTORS = ("ws-x11", "wr-x11", "ws-wl", "wr-wl")


def main() -> bool:
    which = sys.argv[1] if len(sys.argv) > 1 else "ws-x11"
    if which not in SELECTORS:
        raise SystemExit(f"unknown selector {which!r}; one of {SELECTORS}")
    transport, backend = which.split("-")
    mode = "websockets" if transport == "ws" else "webrtc"
    wayland = backend == "wl"
    res = H.Results(f"encoders-{which}")
    for block in (block_striped, block_cpu, block_switch):
        block(mode, wayland, res)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
