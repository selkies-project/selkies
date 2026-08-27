#!/usr/bin/env python3
"""Client behaviours that must not differ by transport, driven from a dpr-2 browser.

Resolution: an auto-mode HiDPI client asks for the window's physical size, a
manual preset is requested as exact framebuffer pixels, reset-to-window returns
to the physical window size, and turning "scale locally" on in auto mode leaves
the window-resize listener armed. Clipboard: a server with the clipboard
disabled must not arm the focus read (Chromium's permission prompt) or send any
clipboard payload. Gamepad: a pad present before the channel opens honours the
persisted gamepad toggle, and one pad's disconnect does not stop polling the
others.

The checks read the wire: r,WxH / js,* / cw,cb messages are tapped at
WebSocket.send and RTCDataChannel.send inside the page, clipboard reads at
Clipboard.prototype, and the X root size confirms what the server realized.

Both cores are checked: the websockets core is the reference the checks were
written against, and the webrtc core must match it.

Usage: python test_core_parity.py [webrtc|websockets|all]
"""
import os
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
RESIZED_W, RESIZED_H = 1100, 680

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


def resolution_block(page: Any, mode: str, res: "H.Results") -> None:
    """Auto -> preset -> reset -> scale-locally + resize, at dpr 2."""
    phys_w, phys_h = VIEW_W * DPR, VIEW_H * DPR
    # The first realization rides the whole cold path; loaded runners stretch it.
    realized = wait_root(phys_w, phys_h, timeout=30)
    res.check("auto mode requests the physical window size",
              root_matches(realized, phys_w, phys_h), f"root={realized}")
    sent = page.evaluate("window.__resSent")
    res.check("auto request on the wire is WxH * dpr", f"{phys_w}x{phys_h}" in sent, sent)

    seen = len(sent)
    post(page, {"type": "setManualResolution", "width": PRESET_W, "height": PRESET_H})
    sent = wait_new_request(page, seen)
    res.check("manual preset is requested as exact pixels",
              len(sent) > seen and sent[-1] == f"{PRESET_W}x{PRESET_H}", sent[seen:])
    realized = wait_root(PRESET_W, PRESET_H)
    res.check("manual preset realized on the server",
              root_matches(realized, PRESET_W, PRESET_H), f"root={realized}")

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
    dismiss it. Focus, key events and IME composition are unaffected by these two
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
                clipboard_enabled_block(page, res)
                soft_keyboard_block(page, res)
                page.context.close()
                gamepad_block(browser, mode, res)
            finally:
                browser.close()
        H.server_start(mode=mode, extra_env={"SELKIES_ENABLE_CLIPBOARD": "false"})
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            try:
                clipboard_disabled_block(browser, mode, res)
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
