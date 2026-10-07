#!/usr/bin/env python3
"""Two controllers on one display: a page of another tab joins the display's
stream beside its owner instead of taking it over, on both transports. Both
stream and both drive input; the owner keeps sizing the display, a reload of
the owner's own page takes its connection back without touching the other, and
once the owner is gone for good the page beside it owns the display and sets
its size.

The ``settings`` and ``settings-wr`` blocks follow the display's stream
settings between the two, over WebSockets and over WebRTC: the page beside the
owner starts on what the owner streams with, whatever it has stored, a setting
its user picks changes the display for both, and the owner's own later picks
leave it be.

The ``nodisrupt`` block holds the page beside the owner to its own stream over
WebSockets: its audio starts and stops for itself alone, and its hidden tab
pauses its own feed, while the owner's audio and video go on as the owner set
them. Once the owner is gone for good, that page owns the display with full
control: hidden, it stops the stream nobody else watches.

Usage: python3 tests/e2e/test_multi_controller.py [websockets|webrtc|settings|settings-wr|nodisrupt|all]
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
import test_encoders as TENC
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


def open_page(pw, seed: dict, storage: dict = None, mode: str = "websockets") -> tuple:
    """A controller page of its own browser context, with `seed` stored before its first load;
    `(browser, page, console lines)`."""
    browser = C.launch_browser(pw, "chromium")
    ctx = browser.new_context(storage_state=storage, viewport={"width": 1280, "height": 720},
                              device_scale_factor=1)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(NAV_JS)
    ctx.add_init_script(C.PAGE_TAP_JS + C.PC_TAP_JS)
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


def wr_codec(page) -> str:
    """The codec the page's WebRTC video arrives in, as its inbound stats name it."""
    state = C.wr_video_state(page)
    if not isinstance(state, dict):
        return ""
    return next((i.get("codec") or "" for pc in state["pcs"] for i in pc["inbound"] if i.get("kind") == "video"), "")


def wait_codec(page, name: str, timeout: float = 30) -> str:
    """Poll the page's inbound video codec until it names `name`; the last one read."""
    deadline = time.time() + timeout
    codec = wr_codec(page)
    while name not in codec and time.time() < deadline:
        time.sleep(1)
        codec = wr_codec(page)
    return codec


def run_settings_wr(res: "H.Results") -> None:
    H.server_start(mode="webrtc")
    try:
        with sync_playwright() as pw:
            owner_browser, owner, owner_lines = open_page(pw, {"encoder": "vp8enc", "video_crf": "30"},
                                                          mode="webrtc")
            res.check("settings-wr: the owner streams", video(owner, "webrtc"))
            codec = wait_codec(owner, "VP8")
            res.check("settings-wr: on the encoder it picked", "VP8" in codec, codec)
            other_browser, other, other_lines = open_page(pw, {"encoder": "h264enc", "video_crf": "20"},
                                                          mode="webrtc")
            res.check("settings-wr: a page of another tab streams too", video(other, "webrtc"))
            state = wait_state(other, lambda s: s["stored"] == "vp8enc" and s["crf"] == "30")
            res.check("settings-wr: and starts on what the owner streams with, whatever it stored",
                      state["stored"] == "vp8enc" and state["crf"] == "30", state)
            time.sleep(5)
            codec = wr_codec(owner)
            res.check("settings-wr: while the display keeps the owner's encoder", "VP8" in codec, codec)

            pick(other, {"encoder": "h264enc"})
            codec = wait_codec(owner, "H264")
            res.check("settings-wr: a pick on the page beside the owner changes the display", "H264" in codec, codec)
            state = wait_state(owner, lambda s: s["stored"] == "h264enc")
            res.check("settings-wr: and the owner follows it", state["stored"] == "h264enc", state)
            res.check("settings-wr: both go on streaming", streaming(owner, "webrtc") and streaming(other, "webrtc"))

            pick(owner, {"video_crf": 35})
            state = wait_state(other, lambda s: s["crf"] == "35")
            res.check("settings-wr: the owner's own later pick reaches the page beside it", state["crf"] == "35", state)
            time.sleep(3)
            codec = wr_codec(owner)
            res.check("settings-wr: and leaves the encoder picked beside it be", "H264" in codec, codec)
            for name, page in (("owner", owner), ("page beside it", other)):
                state = page.evaluate(STATE_JS)
                res.check(f"settings-wr: the {name} never reloaded", state["navs"] == 1, state["navs"])
            storage = owner.context.storage_state()
            other_browser.close()
            owner_browser.close()
        H.server_start(mode="webrtc")
        with sync_playwright() as pw:
            browser, page, _ = open_page(pw, {}, storage=storage, mode="webrtc")
            res.check("settings-wr: a later visit of the owner's browser streams", video(page, "webrtc"))
            state = page.evaluate(STATE_JS)
            codec = wait_codec(page, "VP8")
            res.check("settings-wr: on its own picks again", "VP8" in codec and state["stored"] == "vp8enc"
                      and state["crf"] == "35", (codec, state))
            browser.close()
    finally:
        H.server_stop()


def control(page, pipeline: str, enabled: bool) -> None:
    """What a dashboard posts for its user's pipeline toggle."""
    page.evaluate("([p, on]) => window.postMessage({type: 'pipelineControl', pipeline: p, enabled: on},"
                  " window.location.origin)", [pipeline, enabled])


def frames(page) -> int:
    """The video frames the page has received."""
    return page.evaluate("(window.__wsTypes || {})[4] || 0")


def gained(page, seconds: float = 2.5) -> int:
    """How many video frames the page receives over the next `seconds`."""
    before = frames(page)
    time.sleep(seconds)
    return frames(page) - before


def hears(page, timeout: float = 10) -> bool:
    """Whether audio fills the page's playback buffer within `timeout`."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if (page.evaluate("window.currentAudioBufferSize || 0") or 0) > 0:
            return True
        time.sleep(0.25)
    return False


def silent(page, settle: float = 2.0, span: float = 2.5) -> bool:
    """Whether the page's playback buffer, drained for `settle`, stays empty for `span`."""
    time.sleep(settle)
    deadline = time.time() + span
    while time.time() < deadline:
        if (page.evaluate("window.currentAudioBufferSize || 0") or 0) > 0:
            return False
        time.sleep(0.25)
    return True


def states(page) -> list:
    """The audio and video state messages the server has sent the page."""
    return page.evaluate("window.__wsStates || []")


def hide(page, hidden: bool) -> None:
    """Hide or show the page's tab, as the browser tells it."""
    page.evaluate("""(h) => {
      Object.defineProperty(document, 'hidden', {configurable: true, get: () => h});
      Object.defineProperty(document, 'visibilityState', {configurable: true, get: () => h ? 'hidden' : 'visible'});
      document.dispatchEvent(new Event('visibilitychange'));
    }""", hidden)


def run_nodisrupt(res: "H.Results") -> None:
    H.server_start(mode="websockets")
    tone = H.pulse_sine()
    picture = TENC.Picture(False)
    try:
        with sync_playwright() as pw:
            owner_browser, owner, _ = open_page(pw, {})
            res.check("nodisrupt: the owner streams", video(owner, "websockets"))
            owner.mouse.click(640, 360)
            res.check("nodisrupt: and hears the session", hears(owner))
            control(owner, "audio", False)
            res.check("nodisrupt: until its user turns its audio off", silent(owner))
            told = len(states(owner))
            other_browser, other, _ = open_page(pw, {})
            res.check("nodisrupt: a page of another tab streams too", video(other, "websockets"))
            other.mouse.click(640, 360)
            res.check("nodisrupt: and hears the session by its own start", hears(other))
            res.check("nodisrupt: while the owner's audio stays off", silent(owner, settle=0)
                      and "AUDIO_STARTED" not in states(owner)[told:], states(owner)[told:])
            control(owner, "audio", True)
            res.check("nodisrupt: until the owner turns it back on", hears(owner))
            told = len(states(owner))
            control(other, "audio", False)
            res.check("nodisrupt: the page beside it turning its audio off stops its own",
                      silent(other), states(other)[-2:])
            res.check("nodisrupt: and leaves the owner's playing", hears(owner, timeout=3)
                      and "AUDIO_STOPPED" not in states(owner)[told:], states(owner)[told:])

            hide(other, True)
            time.sleep(1.5)
            before_owner, before_other = frames(owner), frames(other)
            picture.paint()
            time.sleep(2.5)
            res.check("nodisrupt: the page beside it hidden gets no video",
                      frames(other) == before_other, frames(other) - before_other)
            res.check("nodisrupt: while the owner's goes on", frames(owner) > before_owner,
                      frames(owner) - before_owner)
            told = len(states(other))
            hide(other, False)
            time.sleep(2)
            picture.clear()
            res.check("nodisrupt: shown again, it resumes on a fresh start",
                      "PIPELINE_RESETTING primary" in states(other)[told:] and gained(other) > 0,
                      states(other)[told:])

            owner_browser.close()
            res.check("nodisrupt: once the owner is gone for good, the page beside it owns the display",
                      C.wait_log("The controller beside 'primary' owns it now.", timeout=25))
            time.sleep(3)
            hide(other, True)
            res.check("nodisrupt: with full control: hidden, it stops the stream",
                      C.wait_log("Received STOP_VIDEO for 'primary'. Stopping stream.", timeout=10))
            hide(other, False)
            res.check("nodisrupt: and shown, it starts it again", C.wait_log(
                "Received START_VIDEO for 'primary'. Starting its stream.", timeout=10) and gained(other) > 0)
            other_browser.close()
    finally:
        picture.clear()
        H.pulse_unload(tone)
        H.server_stop()


def main() -> int:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    res = H.Results(f"multi-controller-{which}")
    for mode in ("websockets", "webrtc"):
        if which in ("all", mode):
            run(res, mode)
    if which in ("all", "settings"):
        run_settings(res)
    if which in ("all", "settings-wr"):
        run_settings_wr(res)
    if which in ("all", "nodisrupt"):
        run_nodisrupt(res)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
