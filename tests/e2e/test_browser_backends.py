#!/usr/bin/env python3
"""Firefox and WebKit on the cells test_browsers.py leaves out: the Wayland
backend, and WebKit over WebRTC.

firefox-ws-wl / webkit-ws-wl:
    The WebSocket stream comes up on the Wayland backend and a key press and a
    pointer move from the page reach the in-process compositor's seat.
firefox-wr-wl:
    The same over WebRTC; Firefox negotiates H.264 only with the OpenH264 GMP
    plugin that tests/tools/fetch-openh264.sh side-loads into the e2e profile,
    so the cell skips (counted) without it.
webkit-wr-x11:
    WebKit over WebRTC against the X test display; input is observed in the X
    keymap and pointer. A WebKit build that advertises no H.264 receive
    capability, or that receives the RTP but never decodes a frame, skips the
    stream-dependent checks with that reason rather than failing them.

Every cell records what the engine cannot do here as a counted SKIP rather
than a pass, so coverage claims stay honest.

Usage: python3 tests/e2e/test_browser_backends.py [<cell>|all]
"""
import os
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_browsers as TB
from playwright.sync_api import sync_playwright

WL_SOCKET = "wayland-1"
CELLS = ("firefox-ws-wl", "webkit-ws-wl", "firefox-wr-wl", "webkit-wr-x11")

# Counts WebSocket binary frames (headless rAF throttling zeroes the client's
# own fps) and keeps every RTCPeerConnection reachable for getStats.
PAGE_JS = """
  window.__wsFrames = 0;
  (() => {
    const WS = window.WebSocket;
    window.WebSocket = function(...a) {
      const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
      s.addEventListener('message', (e) => { if (e.data instanceof ArrayBuffer) window.__wsFrames++; });
      return s;
    };
    window.WebSocket.prototype = WS.prototype;
    Object.setPrototypeOf(window.WebSocket, WS);
  })();
  window.__pcs = [];
  if (window.RTCPeerConnection) {
    const Orig = window.RTCPeerConnection;
    const Wrapped = function(...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    Wrapped.prototype = Orig.prototype;
    Object.setPrototypeOf(Wrapped, Orig);
    window.RTCPeerConnection = Wrapped;
  }
"""

VIDEO_RTP_JS = """async () => {
  const out = {received: 0, decoded: 0, codec: null};
  for (const pc of window.__pcs || []) {
    const rep = await pc.getStats();
    const byId = {};
    rep.forEach(r => byId[r.id] = r);
    rep.forEach(r => {
      if (r.type === 'inbound-rtp' && (r.kind === 'video' || r.mediaType === 'video')) {
        out.received += r.packetsReceived || 0;
        out.decoded += r.framesDecoded || 0;
        const c = byId[r.codecId];
        if (c) out.codec = c.mimeType;
      }
    });
  }
  return out;
}"""


def cell_parts(cell: str) -> tuple:
    """`(engine, mode, wayland)` for `<engine>-<ws|wr>-<x11|wl>`."""
    engine, transport, backend = cell.split("-")
    return engine, ("webrtc" if transport == "wr" else "websockets"), backend == "wl"


def h264_receivable(page: Any) -> bool:
    """Whether the engine lists H.264 among its RTP receive codecs."""
    return bool(page.evaluate("""() => {
      try { return RTCRtpReceiver.getCapabilities('video').codecs.some(c => /H264/i.test(c.mimeType)); }
      catch (e) { return false; }
    }"""))


def wait_video(page: Any, mode: str) -> Optional[dict]:
    if mode == "webrtc":
        return C.wait_wr_video(page, timeout=45)
    return C.wait_ws_video(page, timeout=25)


def input_checks(res: "H.Results", page: Any, wayland: bool, wl_obs: Any) -> None:
    """A key press and a pointer move from the page, observed at the server's
    seat (compositor) or X server."""
    page.mouse.click(640, 360)
    time.sleep(0.5)
    pressed = False
    for _ in range(4):
        # WebKit headless can drop a synthetic keydown on a throttled render
        # round; repeat the whole press rather than wait on one that never left.
        page.keyboard.down("x")
        time.sleep(0.8)
        if wayland:
            pressed = wl_obs.wait_for("kbd_key", state=1, timeout=3) is not None
        else:
            pressed = C.x11_keymap_pressed("x") is True
        if pressed:
            break
        page.keyboard.up("x")
        time.sleep(0.6)
    res.check("input: key press reached the " + ("wayland seat" if wayland else "X keymap"), pressed)
    page.keyboard.up("x")
    time.sleep(0.5)
    if wayland:
        released = wl_obs.wait_for("kbd_key", state=0, timeout=4) is not None
    else:
        released = C.x11_keymap_pressed("x") is False
    res.check("input: key release observed", released)
    if wayland:
        # Pointer events reach the focused surface: the observer is fullscreen
        # on the output, so any in-stream position lands on it.
        page.mouse.move(8, 8)
        time.sleep(0.6)
        ev = wl_obs.wait_for("ptr_motion", timeout=4)
        res.check("input: pointer motion reached the wayland seat", ev is not None and "x" in (ev or {}), ev)
    else:
        page.mouse.move(200, 150)
        time.sleep(0.8)
        after = C.x11_mouse_pos()
        res.check("input: pointer moved in X (200,150)",
                  abs(after[0] - 200) <= 4 and abs(after[1] - 150) <= 4, after)


def cell_block(cell: str) -> "H.Results":
    engine, mode, wayland = cell_parts(cell)
    res = H.Results(cell)
    H.server_start(mode=mode, wayland=wayland)
    with sync_playwright() as pw:
        browser, ctx = TB.engine_launch(pw, engine)
        ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
        ctx.add_init_script(PAGE_JS)
        page = ctx.pages[0] if (engine == "firefox" and ctx.pages) else ctx.new_page()
        console_errors = []
        page.on("console", lambda m: console_errors.append(m.text) if m.type == "error" else None)
        page.on("pageerror", lambda e: console_errors.append(str(e)))
        not_found = []
        page.on("response", lambda r: not_found.append(r.url) if r.status == 404 else None)
        page.goto(H.BASE_URL, wait_until="load")
        time.sleep(2.0 if engine == "webkit" else 6.0)
        wl_obs = None
        try:
            if mode == "webrtc" and not h264_receivable(page):
                for name in ("video: <video> receiving", "input: key press", "input: pointer"):
                    res.skip(name, f"{engine} lists no H.264 RTP receive codec here")
                return res
            info = wait_video(page, mode)
            if mode == "webrtc" and info is None and engine == "webkit":
                rtp = page.evaluate(VIDEO_RTP_JS)
                if rtp["received"] > 0 and rtp["decoded"] == 0:
                    # The offer was accepted and RTP arrives, but this build has
                    # no decoder behind its advertised H.264: nothing to assert.
                    for name in ("video: <video> receiving", "input: key press", "input: pointer"):
                        res.skip(name, f"WebKit received {rtp['received']} video packets ({rtp['codec']}) "
                                       "but decoded none: no H.264 decoder in this build")
                    return res
                res.check("video: <video> receiving", False, rtp)
            elif mode == "webrtc":
                res.check("video: <video> receiving", info is not None, info)
            else:
                res.check("video: canvas painted", info is not None, info or "no canvas>=640")
                deadline = time.time() + 12
                frames = 0
                while time.time() < deadline:
                    frames = page.evaluate("window.__wsFrames") or 0
                    if frames >= 24:
                        break
                    time.sleep(0.5)
                res.check("video: WS frames flowing", frames >= 24, frames)
            if wayland:
                wl_obs = H.WlObs(WL_SOCKET)
                res.check("wl observer mapped", wl_obs.ready())
            input_checks(res, page, wayland, wl_obs)
            real_errors = [e for e in console_errors if not any(p in e for p in TB.DECODER_ERROR_PATTERNS)]
            benign = [u for u in not_found if u.endswith("/manifest.json") or "favicon" in u]
            bad404 = [u for u in not_found if u not in benign]
            res.check("no console errors (filtered)", not real_errors, "; ".join(real_errors)[:160])
            res.check("no unexpected 404s", not bad404, bad404[:2])
        finally:
            if wl_obs is not None:
                wl_obs.stop()
            if browser:
                browser.close()
            else:
                ctx.close()
    return res


def main() -> None:
    """Run the cells named on argv (default: all)."""
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which != "all" and which not in CELLS:
        print(f"unknown cell {which!r}; one of {', '.join(CELLS)}", file=sys.stderr)
        sys.exit(2)
    blocks = []
    try:
        for cell in CELLS:
            if which not in ("all", cell):
                continue
            if cell == "firefox-wr-wl" and not TB.openh264_version():
                reason = (f"firefox webrtc: no OpenH264 GMP plugin in {TB.FF_E2E_PROFILE}; "
                          "run tests/tools/fetch-openh264.sh to cover H.264 in Firefox")
                if which == cell:
                    H.skip_suite(reason)
                res = H.Results(cell)
                res.skip("video: <video> receiving", reason)
                blocks.append(res)
            else:
                blocks.append(cell_block(cell))
            blocks[-1].summary()
    finally:
        H.server_stop()
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    skipped = sum(len(b.skipped) for b in blocks)
    print(f"\n=== BROWSER-BACKENDS: {total - failed}/{total} passed, {skipped} skipped ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
