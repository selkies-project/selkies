#!/usr/bin/env python3
"""Two controllers on one display: a page of another tab joins the display's
stream beside its owner instead of taking it over, on both transports. Both
stream and both drive input; the owner keeps sizing the display, a reload of
the owner's own page takes its connection back without touching the other, and
once the owner is gone for good the page beside it owns the display and sets
its size.

The ``settings`` block follows the display's stream settings between the two
over WebSockets: the page beside the owner starts on what the owner streams
with, whatever it has stored, a setting its user picks changes the display for
both, and the owner's own later picks leave it be.

Usage: python3 tests/e2e/test_multi_controller.py [websockets|webrtc|settings|all]
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright


def streaming(page, mode: str, seconds: float = 2.0) -> bool:
    """Whether the page receives video over the next `seconds`: WebSockets
    chunks, or the WebRTC video element's clock moving on."""
    probe = ("window.__wsFrames || 0" if mode == "websockets"
             else "(document.querySelector('video') || {currentTime: 0}).currentTime")
    before = page.evaluate(probe)
    time.sleep(seconds)
    return page.evaluate(probe) > before


def video(page, mode: str) -> bool:
    """Whether the page shows video within its timeout."""
    return bool(C.wait_ws_video(page, timeout=30) if mode == "websockets" else C.wait_wr_video(page, timeout=30))


def terminated(page) -> bool:
    """Whether the page shows the server's fatal verdict."""
    return page.evaluate("(document.body.innerText || '').includes('Connection Terminated')")


def wait_root(want: tuple, timeout: float = 20, slack: int = 16) -> tuple:
    """Poll the root size until it is within `slack` pixels of `want`; the last read."""
    deadline = time.time() + timeout
    size = H.x_root_size()
    while time.time() < deadline:
        size = H.x_root_size()
        if all(abs(g - w) <= slack for g, w in zip(size, want)):
            break
        time.sleep(0.5)
    return size


def run(res: "H.Results", mode: str) -> None:
    H.server_start(mode=mode)
    try:
        with sync_playwright() as pw:
            owner_browser, owner, _, _ = C.launch_chrome(pw, mode=mode)
            res.check(f"{mode}: the owner streams", video(owner, mode))
            owner_size = wait_root((1280, 720))
            other_browser, other, _, _ = C.launch_chrome(pw, mode=mode)
            res.check(f"{mode}: a page of another tab streams too", video(other, mode))
            time.sleep(2.0)
            res.check(f"{mode}: and the owner goes on streaming, not terminated",
                      not terminated(owner) and streaming(owner, mode))
            res.check(f"{mode}: nor is the page beside it", not terminated(other) and streaming(other, mode))

            other.mouse.move(200, 150)
            time.sleep(0.5)
            first = C.x11_mouse_pos()
            other.mouse.move(600, 400)
            time.sleep(0.5)
            second = C.x11_mouse_pos()
            res.check(f"{mode}: the page beside it drives the pointer",
                      second[0] - first[0] > 200 and second[1] - first[1] > 100, f"{first} -> {second}")

            other.set_viewport_size({"width": 1024, "height": 640})
            time.sleep(3.0)
            res.check(f"{mode}: its resize leaves the display at the owner's size", H.x_root_size() == owner_size,
                      f"{H.x_root_size()} against {owner_size}")

            owner.reload(wait_until="load")
            res.check(f"{mode}: the owner's reload streams again", video(owner, mode))
            time.sleep(2.0)
            res.check(f"{mode}: and takes its own connection back, leaving the page beside it be",
                      not terminated(other) and streaming(other, mode) and streaming(owner, mode))

            owner_browser.close()
            size = wait_root((1024, 640), timeout=25)
            res.check(f"{mode}: once the owner is gone for good, the page beside it sizes the display",
                      all(abs(g - w) <= 16 for g, w in zip(size, (1024, 640))), size)
            res.check(f"{mode}: and still streams", streaming(other, mode))
            other_browser.close()
    finally:
        H.server_stop()


NAV_JS = "sessionStorage.setItem('__navs', String(Number(sessionStorage.getItem('__navs') || 0) + 1));"
STORAGE_PREFIX_JS = "(location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_')"
SEED_JS = """(() => {
  if (sessionStorage.getItem('__seeded')) return;
  sessionStorage.setItem('__seeded', '1');
  const app = %s;
  for (const [k, v] of Object.entries(%s)) localStorage.setItem(app + '_' + k, v);
})();"""
STATE_JS = """(() => {
  const app = %s;
  return {encoder: window.encoder || null, codec: (window.stream_info && window.stream_info.codec) || null,
          stored: localStorage.getItem(app + '_encoder'), crf: localStorage.getItem(app + '_video_crf'),
          navs: Number(sessionStorage.getItem('__navs') || 0)};
})()""" % STORAGE_PREFIX_JS


def open_page(pw, seed: dict, storage: dict = None) -> tuple:
    """A controller page of its own browser context, with `seed` stored before its first load;
    `(browser, page, console lines)`."""
    browser = C.launch_browser(pw, "chromium")
    ctx = browser.new_context(storage_state=storage, viewport={"width": 1280, "height": 720},
                              device_scale_factor=1)
    ctx.add_init_script(NAV_JS)
    ctx.add_init_script(C.PAGE_TAP_JS)
    if seed:
        ctx.add_init_script(SEED_JS % (STORAGE_PREFIX_JS, json.dumps(seed)))
    page = ctx.new_page()
    lines = []
    page.on("console", lambda m: lines.append(m.text))
    page.goto(H.BASE_URL + "/", wait_until="load")
    return browser, page, lines


def pick(page, settings: dict) -> None:
    """What a dashboard posts for its user's pick."""
    page.evaluate("(s) => window.postMessage({type: 'settings', settings: s}, window.location.origin)", settings)


def wait_state(page, predicate, timeout: float = 20) -> dict:
    """Poll the page's state until `predicate` holds; the last state read."""
    deadline = time.time() + timeout
    state = page.evaluate(STATE_JS)
    while not predicate(state) and time.time() < deadline:
        time.sleep(0.5)
        state = page.evaluate(STATE_JS)
    return state


def crashed(lines: list) -> list:
    """The console lines of a decoder fallback or crash."""
    return [line for line in lines if "FATAL DECODER" in line or "Primary client fallback" in line]


def run_settings(res: "H.Results") -> None:
    H.server_start(mode="websockets")
    try:
        with sync_playwright() as pw:
            owner_browser, owner, owner_lines = open_page(pw, {"encoder": "jpeg", "video_crf": "30"})
            res.check("settings: the owner streams", video(owner, "websockets"))
            state = wait_state(owner, lambda s: s["codec"] == "jpeg")
            res.check("settings: on the encoder it picked", state["codec"] == "jpeg", state)
            other_browser, other, other_lines = open_page(pw, {"encoder": "h264enc", "video_crf": "20"})
            res.check("settings: a page of another tab streams too", video(other, "websockets"))
            state = wait_state(other, lambda s: s["encoder"] == "jpeg" and s["crf"] == "30")
            res.check("settings: and starts on what the owner streams with, whatever it stored",
                      state["encoder"] == "jpeg" and state["stored"] == "jpeg" and state["crf"] == "30", state)
            time.sleep(10)
            state = other.evaluate(STATE_JS)
            res.check("settings: without a decoder fallback or a reload",
                      state["navs"] == 1 and not crashed(other_lines), (state["navs"], crashed(other_lines)))
            state = owner.evaluate(STATE_JS)
            res.check("settings: while the display keeps the owner's encoder", state["codec"] == "jpeg", state)

            pick(other, {"encoder": "h264enc"})
            state = wait_state(owner, lambda s: s["codec"] == "h264" and s["encoder"] == "h264enc")
            res.check("settings: a pick on the page beside the owner changes the display",
                      state["codec"] == "h264", state)
            res.check("settings: and the owner follows it", state["encoder"] == "h264enc" and state["stored"] == "h264enc",
                      state)
            res.check("settings: both go on streaming", streaming(owner, "websockets") and streaming(other, "websockets"))

            pick(owner, {"video_crf": 35})
            state = wait_state(other, lambda s: s["crf"] == "35")
            res.check("settings: the owner's own later pick reaches the page beside it", state["crf"] == "35", state)
            time.sleep(3)
            state = owner.evaluate(STATE_JS)
            res.check("settings: and leaves the encoder picked beside it be", state["codec"] == "h264", state)
            for name, page, lines in (("owner", owner, owner_lines), ("page beside it", other, other_lines)):
                state = page.evaluate(STATE_JS)
                res.check(f"settings: the {name} never fell back or reloaded",
                          state["navs"] == 1 and not crashed(lines), (state["navs"], crashed(lines)))
            storage = owner.context.storage_state()
            other_browser.close()
            owner_browser.close()
        # A later visit of the owner's browser, the display's last session gone: its own picks.
        H.server_start(mode="websockets")
        with sync_playwright() as pw:
            browser, page, _ = open_page(pw, {}, storage=storage)
            res.check("settings: a later visit of the owner's browser streams", video(page, "websockets"))
            state = wait_state(page, lambda s: s["codec"] == "jpeg")
            res.check("settings: on its own picks again", state["codec"] == "jpeg" and state["stored"] == "jpeg"
                      and state["crf"] == "35", state)
            browser.close()
    finally:
        H.server_stop()


def main() -> int:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    res = H.Results(f"multi-controller-{which}")
    for mode in ("websockets", "webrtc"):
        if which in ("all", mode):
            run(res, mode)
    if which in ("all", "settings"):
        run_settings(res)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
