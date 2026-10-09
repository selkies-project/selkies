#!/usr/bin/env python3
"""Client behaviors that must not differ by transport, driven from a dpr-2 browser.

Resolution: an auto-mode HiDPI client asks for the window's physical size, a
manual preset is requested as exact framebuffer pixels and shown, with "scale
locally" off, at one stream pixel per device pixel, reset-to-window returns to
the physical window size, and turning "scale locally" on in auto mode leaves
the window-resize listener armed. The WebRTC video is drawn nearest-sampled
exactly while it is shown 1:1, and smoothed once scaled to fit. A pixel ratio
changed through DevTools emulation re-requests the stream at it. HiDPI: the
flag either streams physical pixels and scales the desktop, or leaves the
desktop unscaled and divides the request by the UI-scaling pick for the
browser to stretch back — never both. Clipboard: a server with the clipboard
disabled must not arm the focus read (Chromium's permission prompt) or send any
clipboard payload. Gamepad: a pad present before the channel opens honors the
persisted gamepad toggle, and one pad's disconnect does not stop polling the
others. Resize policy: with dynamic resizing disabled a manual resolution
posted to the primary's page is neither requested nor applied, and the
desktop keeps its size.
Rendering: changing anti-aliasing applies to the visible sink immediately,
including a static desktop that sends no new video frames.

The checks read the wire: r,WxH / js,* / cw,cb messages are tapped at
WebSocket.send and RTCDataChannel.send inside the page, clipboard reads at
Clipboard.prototype, and the X root size confirms what the server realized.

Both cores are checked: the websockets core is the reference the checks were
written against, and the webrtc core must match it.

Usage: python test_core_parity.py [webrtc|websockets|all]
"""
import os
import subprocess
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

SELECTORS = ("webrtc", "websockets")
# CSS-px viewport of the dpr-2 browser: the physical size the auto path must
# request is twice this.
VIEW_W, VIEW_H = 1000, 700
DPR = 2
PRESET_W, PRESET_H = 1280, 720
# A size X11's modes round up (to 8-pixel cells), which the stream still carries exactly.
ODD_W, ODD_H = 1278, 712
RESIZED_W, RESIZED_H = 1100, 680
# A manual resolution the local density would never ask for, and the pick it
# derives: its shorter side against the 1080 rows 96 DPI is for. Neither 96 nor
# the dpr's 192, so the desktop's DPI tells the two rules apart.
MANUAL_W, MANUAL_H = 2560, 1440
MANUAL_DPI = 120

WIRE_TAP = """
(() => {
  window.__resSent = [];
  window.__padSent = [];
  window.__clipSent = 0;
  window.__clipReads = 0;
  const tap = (d) => {
    if (typeof d !== 'string') return;
    if (d.startsWith('r,')) window.__resSent.push(d.split(',')[1]);
    else if (d.startsWith('js,')) window.__padSent.push(d.split(',')[1]);
    else if (d.startsWith('cw') || d.startsWith('cb')) window.__clipSent++;
  };
  // The websockets transport runs its socket in a worker, so its sends are
  // observed through the page-side handle rather than WebSocket.prototype.
  let transport = null;
  Object.defineProperty(window, 'selkiesTransport', {
    configurable: true,
    get: () => transport,
    set: (v) => {
      transport = v;
      if (v && typeof v.send === 'function' && !v.__tapped) {
        const orig = v.send.bind(v);
        v.send = (d) => { tap(d); return orig(d); };
        v.__tapped = true;
      }
    },
  });
  const protos = [window.RTCDataChannel && RTCDataChannel.prototype,
                  window.WebSocket && WebSocket.prototype];
  for (const proto of protos) {
    if (!proto || typeof proto.send !== 'function') continue;
    const orig = proto.send;
    proto.send = function(d) { tap(d); return orig.call(this, d); };
  }
  const clip = window.Clipboard && Clipboard.prototype;
  if (clip) {
    for (const m of ['read', 'readText']) {
      const orig = clip[m];
      if (typeof orig !== 'function') continue;
      clip[m] = function(...a) { window.__clipReads++; return orig.apply(this, a); };
    }
  }
})();
"""

# Two synthetic W3C pads so a disconnect can take one away while the other
# stays pressable.
PADS_INIT = """
window.__pads = [0, 1].map((i) => ({
  index: i, id: "Selkies Test Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)",
  mapping: "standard", connected: true, timestamp: 1,
  buttons: Array.from({length: 17}, () => ({pressed: false, touched: false, value: 0})),
  axes: [0, 0, 0, 0],
}));
navigator.getGamepads = () => [window.__pads[0], window.__pads[1], null, null];
window.__padPress = (p, i, v) => {
  const pad = window.__pads[p];
  pad.buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
  pad.timestamp = performance.now();
};
"""

# The cores' storage prefix (lib/util.js getStorageAppName): origin + path with
# everything but [A-Za-z0-9._-] replaced by '_'.
STORAGE_APP = "(location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_')"


def new_page(browser: Any, mode: str, extra_init: Optional[list] = None) -> Any:
    """A dpr-2 page on the test server with the wire taps installed.

    Args:
        browser: Playwright browser.
        mode: Transport the page must load (skips the mode-flip probe).
        extra_init: Further init scripts, run before the page's own.

    Returns:
        The page, loaded.
    """
    ctx = browser.new_context(viewport={"width": VIEW_W, "height": VIEW_H},
                              device_scale_factor=DPR, permissions=[])
    try:
        ctx.grant_permissions(["clipboard-read", "clipboard-write"], origin=H.BASE_URL)
    except Exception:
        pass
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(WIRE_TAP)
    for script in extra_init or []:
        ctx.add_init_script(script)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    # Dashboards can constrain media globally, as Wish's Tailwind preflight does.
    page.add_style_tag(content="video { max-width: 100%; height: auto; }")
    return page


def wait_video(page: Any, mode: str) -> Optional[dict]:
    """Decoding video on the page, per transport."""
    return C.wait_wr_video(page) if mode == "webrtc" else C.wait_ws_video(page)


def wait_root(w: int, h: int, timeout: float = 15) -> tuple:
    """Poll the X root until it is within the CVT cell of `w`x`h`.

    Returns:
        The last root size read, for the caller to compare and report.
    """
    deadline = time.time() + timeout
    realized = H.x_root_size()
    while time.time() < deadline:
        realized = H.x_root_size()
        if abs(realized[0] - w) <= 16 and abs(realized[1] - h) <= 16:
            return realized
        time.sleep(0.3)
    return realized


def root_matches(realized: tuple, w: int, h: int) -> bool:
    """Whether a realized root size is within the CVT cell of `w`x`h`."""
    return abs(realized[0] - w) <= 16 and abs(realized[1] - h) <= 16


def wait_dpi(want: int, timeout: float = 10) -> int:
    """Poll the resource database until Xft.dpi reads `want`.

    Returns:
        The last DPI read, 96 where the resource is unset: X's own default,
        which is what an application reads when nothing overrides it.
    """
    env = {**os.environ, "DISPLAY": H.require_display()}
    deadline = time.time() + timeout
    dpi = 96
    while time.time() < deadline:
        out = subprocess.run(["xrdb", "-query"], capture_output=True, text=True,
                             env=env).stdout
        dpi = next((int(float(line.split(":", 1)[1]))
                    for line in out.splitlines() if line.startswith("Xft.dpi")), 96)
        if dpi == want:
            return dpi
        time.sleep(0.3)
    return dpi


def wait_new_request(page: Any, seen: int, timeout: float = 8) -> list:
    """Poll until the page has put more than `seen` r, requests on the wire.

    Returns:
        Every WxH requested so far, in wire order.
    """
    deadline = time.time() + timeout
    sent = page.evaluate("window.__resSent")
    while time.time() < deadline and len(sent) <= seen:
        time.sleep(0.25)
        sent = page.evaluate("window.__resSent")
    return sent


def post(page: Any, message: dict) -> None:
    """Post a dashboard message into the page."""
    page.evaluate("(m) => window.postMessage(m, window.location.origin)", message)


def focus_gesture(page: Any) -> None:
    """The focus event Chromium's local->server clipboard read hangs on."""
    page.bring_to_front()
    page.evaluate("window.dispatchEvent(new Event('focus'))")


def button_reports(page: Any) -> int:
    """Gamepad button reports the page has put on the wire so far."""
    return page.evaluate("window.__padSent.filter((t) => t === 'b').length")


def sink_box(page: Any, mode: str) -> Optional[dict]:
    """Measure the visible sink relative to its container.

    The WebSockets canvas can be hidden behind a video or worker canvas.
    Measure the sink actually displayed: inline dimensions alone do not catch
    a stylesheet clamping the video while leaving its centering offsets intact.

    Returns:
        `width`, `height`, `left`, and `top` in CSS pixels, or None where the
        page has no such element, which a check reports rather than raising.
    """
    sinks = ["stream"] if mode == "webrtc" else ["videoStream", "videoWorkerCanvas", "videoCanvas"]
    return page.evaluate(f"""(() => {{
      const el = {sinks!r}.map(id => document.getElementById(id)).find(
        el => el && getComputedStyle(el).display !== 'none' && el.getBoundingClientRect().width > 0);
      if (!el) return null;
      const box = el.getBoundingClientRect();
      const parent = el.parentElement.getBoundingClientRect();
      return {{width: box.width, height: box.height,
               left: box.left - parent.left, top: box.top - parent.top}};
    }})()""")


def stream_size(page: Any, mode: str, want: tuple, timeout: float = 15) -> Optional[tuple]:
    """The stream's own pixel size, polled until it is `want` or time runs out:
    the WebRTC video's intrinsic size, or the websockets canvas's."""
    probe = ("(() => { const v = document.getElementById('stream'); return v ? [v.videoWidth, v.videoHeight] : null; })()"
             if mode == "webrtc" else
             "(() => { const c = document.getElementById('videoCanvas'); return c ? [c.width, c.height] : null; })()")
    deadline = time.time() + timeout
    size = None
    while time.time() < deadline:
        got = page.evaluate(probe)
        size = tuple(got) if got else None
        if size == want:
            break
        time.sleep(0.3)
    return size


def video_rendering(page: Any, want: str, timeout: float = 8) -> Optional[str]:
    """The WebRTC video's `image-rendering`, polled until it is `want`: the
    rule reruns when the stream's own size settles after the box."""
    deadline = time.time() + timeout
    got = None
    while time.time() < deadline:
        got = page.evaluate("(() => { const v = document.getElementById('stream'); "
                            "return v ? v.style.imageRendering : null; })()")
        if got == want:
            break
        time.sleep(0.3)
    return got


def static_rendering_block(page: Any, mode: str, res: "H.Results") -> None:
    """Change the visible filter in both directions while the desktop is idle.

    A canvas hidden behind a video can hold the requested filter even while
    the visible sink keeps its previous one. Read the video and its presented
    frame count, so a later frame cannot make a stale filter appear correct.
    """
    post(page, {"type": "setManualResolution", "width": PRESET_W, "height": PRESET_H})
    stream_size(page, mode, (PRESET_W, PRESET_H))
    post(page, {"type": "setScaleLocally", "value": True})
    post(page, {"type": "setAntiAliasing", "value": True})
    post(page, {"type": "settings", "settings": {
        "video_streaming_mode": False, "use_paint_over_quality": False}})
    probe = """() => {
      const v = [...document.querySelectorAll('video')].find(
        v => getComputedStyle(v).display !== 'none' && v.videoWidth > 0);
      return v ? {rendering: getComputedStyle(v).imageRendering,
                  frames: v.getVideoPlaybackQuality().totalVideoFrames} : null;
    }"""
    try:
        deadline = time.monotonic() + 15
        before = None
        while time.monotonic() < deadline:
            before = page.evaluate(probe)
            time.sleep(1)
            after = page.evaluate(probe)
            if before and after and before["frames"] == after["frames"]:
                break
        idle = bool(before and after and before["frames"] == after["frames"])
        res.check("static rendering: the visible video receives no new frames", idle, after)
        if not idle:
            return
        for enabled in (False, True):
            before = page.evaluate(probe)
            post(page, {"type": "setAntiAliasing", "value": enabled})
            time.sleep(0.2)
            after = page.evaluate(probe)
            accepted = ("auto",) if enabled else ("pixelated", "crisp-edges")
            res.check(f"static rendering: anti-aliasing {enabled} reaches the visible video",
                      bool(after and after["rendering"] in accepted), after)
            res.check(f"static rendering: anti-aliasing {enabled} needs no new frame",
                      bool(after and before and after["frames"] == before["frames"]), after)
    finally:
        post(page, {"type": "settings", "settings": {"video_streaming_mode": True}})


def resolution_block(page: Any, mode: str, res: "H.Results") -> None:
    """Auto -> preset -> exact box -> reset -> scale-locally + resize, at dpr 2."""
    phys_w, phys_h = VIEW_W * DPR, VIEW_H * DPR
    # The first realization rides the whole cold path; loaded runners stretch it.
    realized = wait_root(phys_w, phys_h, timeout=30)
    res.check("auto mode requests the physical window size",
              root_matches(realized, phys_w, phys_h), f"root={realized}")
    sent = page.evaluate("window.__resSent")
    res.check("auto request on the wire is WxH * dpr", f"{phys_w}x{phys_h}" in sent, sent)
    if mode == "webrtc":
        got = video_rendering(page, "pixelated")
        res.check("a stream at the window's device pixels is drawn 1:1", got == "pixelated", got)

    seen = len(sent)
    post(page, {"type": "setManualResolution", "width": PRESET_W, "height": PRESET_H})
    sent = wait_new_request(page, seen)
    res.check("manual preset is requested as exact pixels",
              len(sent) > seen and sent[-1] == f"{PRESET_W}x{PRESET_H}", sent[seen:])
    realized = wait_root(PRESET_W, PRESET_H)
    res.check("manual preset realized on the server",
              root_matches(realized, PRESET_W, PRESET_H), f"root={realized}")

    # Scale-locally off is one stream pixel per device pixel.
    post(page, {"type": "setScaleLocally", "value": False})
    time.sleep(0.3)
    box = sink_box(page, mode)
    want_w, want_h = PRESET_W / DPR, PRESET_H / DPR
    fits = bool(box) and (abs(box["width"] - want_w) < 1 and abs(box["height"] - want_h) < 1
                          and box["left"] >= 0 and box["top"] >= 0
                          and box["left"] + box["width"] <= VIEW_W
                          and box["top"] + box["height"] <= VIEW_H)
    res.check("exact box is the preset over the density, inside the viewport",
              fits, f"{box} want {want_w}x{want_h}")
    post(page, {"type": "setUseCssScaling", "value": True})
    time.sleep(0.3)
    off_box = sink_box(page, mode)
    res.check("exact box ignores the HiDPI flag", bool(off_box) and off_box == box,
              f"{off_box} after HiDPI off, {box} before")
    post(page, {"type": "setUseCssScaling", "value": False})
    time.sleep(0.3)
    if mode == "webrtc":
        got = video_rendering(page, "pixelated")
        res.check("an exact manual box is drawn 1:1", got == "pixelated", got)
        post(page, {"type": "setScaleLocally", "value": True})
        got = video_rendering(page, "auto")
        res.check("a manual resolution scaled to fit is smoothed", got == "auto", got)
        post(page, {"type": "setScaleLocally", "value": False})
        time.sleep(0.3)
    sent = page.evaluate("window.__resSent")

    # An exact 4K stream exceeds this viewport even at DPR 2.
    seen = len(sent)
    post(page, {"type": "setManualResolution", "width": 3840, "height": 2160})
    wait_new_request(page, seen)
    size = stream_size(page, mode, (3840, 2160))
    res.check("oversized manual stream is realized", size == (3840, 2160), size)
    for fit in (False, True, False):
        post(page, {"type": "setScaleLocally", "value": fit})
        time.sleep(0.3)
        box = sink_box(page, mode)
        want_w, want_h = (VIEW_W, VIEW_W * 2160 / 3840) if fit else (3840 / DPR, 2160 / DPR)
        expected = {"width": want_w, "height": want_h,
                    "left": (VIEW_W - want_w) / 2, "top": (VIEW_H - want_h) / 2}
        matches = bool(box) and all(abs(box[key] - value) < 1 for key, value in expected.items())
        res.check(f"oversized visible sink follows scale-locally={fit}", matches,
                  f"{box} want {expected}")
    sent = page.evaluate("window.__resSent")

    seen = len(sent)
    post(page, {"type": "setManualResolution", "width": ODD_W, "height": ODD_H})
    wait_new_request(page, seen)
    realized = wait_root(ODD_W, ODD_H)
    size = stream_size(page, mode, (ODD_W, ODD_H))
    res.check("a size the mode rounds up streams at the size asked for",
              size == (ODD_W, ODD_H), f"stream {size}, root {realized}")
    sent = page.evaluate("window.__resSent")

    seen = len(sent)
    post(page, {"type": "resetResolutionToWindow"})
    sent = wait_new_request(page, seen)
    res.check("reset-to-window re-requests the physical window size",
              len(sent) > seen and sent[-1] == f"{phys_w}x{phys_h}", sent[seen:])
    realized = wait_root(phys_w, phys_h)
    res.check("reset-to-window realized on the server",
              root_matches(realized, phys_w, phys_h), f"root={realized}")
    manual = page.evaluate("window.manualResolution || window.manual_resolution || false")
    res.check("reset-to-window leaves manual mode", manual is False, manual)

    seen = len(sent)
    post(page, {"type": "setScaleLocally", "value": True})
    time.sleep(0.3)
    persisted = page.evaluate(f"localStorage.getItem({STORAGE_APP} + '_scaleLocallyManual')")
    res.check("scale-locally choice persisted", persisted == "true", persisted)
    page.set_viewport_size({"width": RESIZED_W, "height": RESIZED_H})
    sent = wait_new_request(page, seen)
    want = f"{RESIZED_W * DPR}x{RESIZED_H * DPR}"
    res.check("scale-locally in auto mode keeps the window resize armed",
              len(sent) > seen and sent[-1] == want, sent[seen:])
    realized = wait_root(RESIZED_W * DPR, RESIZED_H * DPR)
    res.check("window resize realized on the server",
              root_matches(realized, RESIZED_W * DPR, RESIZED_H * DPR), f"root={realized}")


def emulated_density_block(page: Any, mode: str, res: "H.Results") -> None:
    """A pixel ratio changed through DevTools emulation re-requests the stream
    at the new density, as a real display change does, and changing it back
    restores the physical size. Such a change fires no resize and, in
    Chromium, no resolution media query, so only the page's poll of the live
    value sees it."""
    cdp = page.context.new_cdp_session(page)
    events = page.evaluate("""(() => { window.__dprEvents = [];
      addEventListener('resize', () => __dprEvents.push('resize'));
      matchMedia(`(resolution: ${devicePixelRatio}dppx)`).addEventListener('change', () => __dprEvents.push('query'));
      return true; })()""")
    for dsf, want in ((1, f"{RESIZED_W}x{RESIZED_H}"), (DPR, f"{RESIZED_W * DPR}x{RESIZED_H * DPR}")):
        seen = len(page.evaluate("window.__resSent"))
        cdp.send("Emulation.setDeviceMetricsOverride",
                 {"width": RESIZED_W, "height": RESIZED_H, "deviceScaleFactor": dsf, "mobile": False})
        sent = wait_new_request(page, seen)
        fired = page.evaluate("window.__dprEvents.splice(0)") if events else None
        res.check(f"an emulated pixel ratio of {dsf} re-requests the stream at it",
                  len(sent) > seen and sent[-1] == want, f"{sent[seen:]} want {want}; events {fired}")
    # Left attached at the context's own metrics: detaching drops the override,
    # and the page with it to the bare window's size.


def hidpi_block(page: Any, mode: str, res: "H.Results") -> None:
    """The HiDPI flag decides one thing, on either transport.

    On, the stream is the window's physical pixels and the desktop is scaled to
    the UI-scaling pick, so the remote UI comes out the size of the local one.
    Off, the remote UI is not scaled and the pick divides the resolution asked
    for instead, which the browser stretches back: scaling on both sides at
    once drew the remote UI at the pick twice over. A manual resolution is the
    exact framebuffer either way -- a toggle must not swing the number the
    operator asked for -- so there the pick has no request to divide and
    reaches the desktop whatever the flag says, which is one application and
    not two. The pick here is the automatic default, this display's own
    scaling as a DPI.
    """
    pick = DPR * 96
    css_w, css_h = page.evaluate("[window.innerWidth, window.innerHeight]")
    on_dpi = wait_dpi(pick)
    res.check("HiDPI on scales the desktop to the pick", on_dpi == pick,
              f"Xft.dpi={on_dpi} want {pick}")

    seen = len(page.evaluate("window.__resSent"))
    post(page, {"type": "setUseCssScaling", "value": True})
    sent = wait_new_request(page, seen)
    off_request = sent[-1] if len(sent) > seen else ""
    res.check("HiDPI off asks for the window's CSS size",
              off_request == f"{css_w}x{css_h}", sent[seen:])
    off_dpi = wait_dpi(96)
    res.check("HiDPI off leaves the desktop unscaled", off_dpi == 96, f"Xft.dpi={off_dpi}")

    # In auto mode the flag decides sharpness, not size: a window is drawn at
    # the DPI and shown at the CSS box over the stream, so the same ratio
    # either way is the same window on screen -- half the pixels, and the
    # desktop rescaled by nothing.
    on_px, off_px = css_w * DPR, css_w
    res.check("the flag leaves a window the size it had",
              off_dpi * on_px == on_dpi * off_px,
              f"{off_dpi} DPI over {off_px}px vs {on_dpi} over {on_px}px")

    # A manual resolution is the exact framebuffer whatever the flag says, so
    # the pick has no request to divide there and governs the desktop instead:
    # one application, not the two CSS scaling used to make. On its automatic
    # default the pick is that framebuffer's own, since it decides how large
    # the desktop draws its UI and the screen showing it says nothing about it.
    seen = len(sent)
    post(page, {"type": "setManualResolution", "width": MANUAL_W, "height": MANUAL_H})
    sent = wait_new_request(page, seen)
    res.check("HiDPI off asks for a manual resolution exactly",
              len(sent) > seen and sent[-1] == f"{MANUAL_W}x{MANUAL_H}", sent[seen:])
    manual_dpi = wait_dpi(MANUAL_DPI)
    res.check("and the resolution's own pick reaches the desktop there",
              manual_dpi == MANUAL_DPI, f"Xft.dpi={manual_dpi} want {MANUAL_DPI}")

    seen = len(sent)
    post(page, {"type": "resetResolutionToWindow"})
    sent = wait_new_request(page, seen)
    res.check("back on the window size the request is physical again",
              len(sent) > seen and sent[-1] == f"{css_w}x{css_h}", sent[seen:])
    window_dpi = wait_dpi(96)
    res.check("and the desktop is unscaled again", window_dpi == 96,
              f"Xft.dpi={window_dpi}")

    seen = len(sent)
    post(page, {"type": "setUseCssScaling", "value": False})
    sent = wait_new_request(page, seen)
    res.check("HiDPI on again asks for the physical size",
              len(sent) > seen and sent[-1] == f"{css_w * DPR}x{css_h * DPR}", sent[seen:])
    back_dpi = wait_dpi(pick)
    res.check("HiDPI on again scales the desktop", back_dpi == pick,
              f"Xft.dpi={back_dpi} want {pick}")


def clipboard_enabled_block(page: Any, res: "H.Results") -> None:
    """With the clipboard on, the focus gesture reads the local clipboard."""
    enabled = page.evaluate("window.clipboard_enabled")
    res.check("clipboard_enabled mirrored from the server (on)", enabled is True, enabled)
    focus_gesture(page)
    deadline = time.time() + 5
    reads = 0
    while time.time() < deadline and not reads:
        reads = page.evaluate("window.__clipReads")
        time.sleep(0.25)
    res.check("clipboard on: focus gesture reads the local clipboard", reads > 0, reads)


def soft_keyboard_block(page: Any, res: "H.Results") -> None:
    """The overlay collects the stream's taps without opening a soft keyboard.

    It is a real text input laid over the video, so a mobile engine would open its
    keyboard on every tap of the session -- over the picture, and with no way to
    dismiss it. Focus, key events, and IME composition are unaffected by these two
    attributes; the off-screen assist input is what deliberately opens one, so it
    must not carry them.
    """
    overlay = page.evaluate(
        "(() => { const e = document.getElementById('overlayInput');"
        " return e && { mode: e.getAttribute('inputmode'), policy: e.getAttribute('virtualkeyboardpolicy'),"
        " type: e.type, readOnly: e.readOnly }; })()")
    res.check("the stream overlay asks for no virtual keyboard",
              bool(overlay) and overlay.get("mode") == "none", str(overlay))
    res.check("the stream overlay keeps the keyboard the page's to open",
              bool(overlay) and overlay.get("policy") == "manual", str(overlay))
    res.check("the stream overlay stays editable, for IME composition",
              bool(overlay) and overlay.get("readOnly") is False, str(overlay))
    assist = page.evaluate(
        "(() => { const e = document.getElementById('keyboard-input-assist');"
        " return e && { mode: e.getAttribute('inputmode'), type: e.type }; })()")
    res.check("the assist input still opens one on purpose",
              bool(assist) and assist.get("mode") in (None, "text"), str(assist))
    res.check("both cores build the assist input the same way",
              bool(assist) and assist.get("type") == "search", str(assist))


def gamepad_block(browser: Any, mode: str, res: "H.Results") -> None:
    """Persisted toggle before channel open, then a disconnect of one pad."""
    toggle_off = f"localStorage.setItem({STORAGE_APP} + '_isGamepadEnabled', 'false');"
    page = new_page(browser, mode, extra_init=[PADS_INIT, toggle_off])
    try:
        res.check("gamepad page: video flowing", bool(wait_video(page, mode)))
        time.sleep(1.0)
        page.evaluate("window.__padPress(0, 0, 1)")
        time.sleep(0.4)
        page.evaluate("window.__padPress(0, 0, 0)")
        time.sleep(0.6)
        before = button_reports(page)
        res.check("persisted toggle off: a pad present before connect is not polled",
                  before == 0, before)

        post(page, {"type": "gamepadControl", "enabled": True})
        time.sleep(0.3)
        page.evaluate("window.__padPress(0, 1, 1)")
        time.sleep(0.4)
        page.evaluate("window.__padPress(0, 1, 0)")
        time.sleep(0.6)
        after_on = button_reports(page)
        res.check("toggle on: button reports reach the wire", after_on > before, after_on)

        page.evaluate("window.__pads[1] = null; window.dispatchEvent(new Event('gamepaddisconnected'));")
        time.sleep(0.5)
        page.evaluate("window.__padPress(0, 2, 1)")
        time.sleep(0.4)
        page.evaluate("window.__padPress(0, 2, 0)")
        time.sleep(0.6)
        after_dc = button_reports(page)
        res.check("one pad's disconnect keeps the other pad polled",
                  after_dc > after_on, f"{after_on} -> {after_dc}")
    finally:
        page.context.close()


def clipboard_disabled_block(browser: Any, mode: str, res: "H.Results") -> None:
    """enable_clipboard=false: no local read is armed and nothing is sent."""
    page = new_page(browser, mode)
    try:
        res.check("clipboard-off page: video flowing", bool(wait_video(page, mode)))
        time.sleep(1.0)
        enabled = page.evaluate("window.clipboard_enabled")
        res.check("clipboard_enabled mirrored from the server (off)", enabled is False, enabled)
        focus_gesture(page)
        page.evaluate("navigator.clipboard.writeText('parity-probe').catch(() => {})")
        focus_gesture(page)
        post(page, {"type": "clipboardUpdateFromUI", "text": "parity-probe-ui"})
        time.sleep(2.5)
        reads = page.evaluate("window.__clipReads")
        sent = page.evaluate("window.__clipSent")
        res.check("clipboard off: focus gesture reads nothing", reads == 0, reads)
        res.check("clipboard off: no clipboard payload sent", sent == 0, sent)
    finally:
        page.context.close()


def pinned_block(browser: Any, mode: str, res: "H.Results") -> None:
    """enable_resize=false: a manual resolution posted to the primary's page,
    as an embedding front end posts one, is neither requested nor applied, and
    the desktop keeps its size."""
    page = new_page(browser, mode)
    try:
        res.check("pinned page: video flowing", bool(wait_video(page, mode)))
        time.sleep(1.0)
        root = H.x_root_size()
        seen = len(page.evaluate("window.__resSent"))
        post(page, {"type": "setManualResolution", "width": PRESET_W, "height": PRESET_H})
        time.sleep(4.0)
        sent = page.evaluate("window.__resSent")[seen:]
        res.check("pinned: a manual resolution is not requested", not sent, sent)
        manual = page.evaluate("window.manualResolution || window.manual_resolution || false")
        res.check("pinned: the page stays out of manual mode", manual is False, manual)
        after = H.x_root_size()
        res.check("pinned: the desktop keeps its size", after == root, f"{root} -> {after}")
    finally:
        page.context.close()


def run(mode: str) -> bool:
    """Drive every block over one transport; True when all checks passed."""
    res = H.Results(f"core-parity-{mode}")
    H.server_start(mode=mode)
    try:
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            try:
                page = new_page(browser, mode)
                res.check("video flowing", bool(wait_video(page, mode)))
                time.sleep(1.0)
                resolution_block(page, mode, res)
                emulated_density_block(page, mode, res)
                hidpi_block(page, mode, res)
                static_rendering_block(page, mode, res)
                clipboard_enabled_block(page, res)
                soft_keyboard_block(page, res)
                page.context.close()
                gamepad_block(browser, mode, res)
            finally:
                browser.close()
        H.server_start(mode=mode, extra_env={"SELKIES_ENABLE_CLIPBOARD": "false",
                                             "SELKIES_ENABLE_RESIZE": "false"})
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            try:
                clipboard_disabled_block(browser, mode, res)
                pinned_block(browser, mode, res)
            finally:
                browser.close()
    finally:
        H.server_stop()
    return res.summary()


if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    chosen = SELECTORS if which == "all" else (which,)
    ok = True
    for m in chosen:
        ok = run(m) and ok
    sys.exit(0 if ok else 1)
