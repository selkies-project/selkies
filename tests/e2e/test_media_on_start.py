#!/usr/bin/env python3
"""The session start policy end to end, over both transports in Chromium.

A session starts with video, audio and gamepad input on and the microphone
and webcam off; the `*_on_start` settings change that for the session owner's
primary page, and what starts off is not captured at all until the side menu
turns it on. Each block starts a server with one setting flipped and reads
both ends: the page's pipeline status messages and element state, and the
server log for what was (not) captured. Video and audio are also switched on
and off again through the dashboard's control messages, a shared viewer is
shown exempt, and the gamepad block proves a browser's persisted toggle wins.
The capture-gating blocks (defaults, video off, audio off) also run against
the Wayland backend, whose captures start and stop through the compositor.

    python3 tests/e2e/test_media_on_start.py websockets|webrtc|websockets-wl|webrtc-wl|all
"""
import os
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_core_parity as P
import test_microphone as TM
import test_webcam as TW
from playwright.sync_api import sync_playwright

# Records the last state the cores announced per pipeline.
STATUS_JS = """
  window.__pipe = {};
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || (d.type !== 'sidebarButtonStatusUpdate' && d.type !== 'pipelineStatusUpdate')) return;
    for (const k of ['video', 'audio', 'microphone', 'webcam', 'gamepad']) {
      if (d[k] !== undefined) window.__pipe[k] = d[k];
    }
  });
"""

# Log lines that mark a capture starting or stopping, per transport.
VIDEO_STARTED = {"websockets": "Preparing to start capture for display='primary'",
                 "webrtc": "Started screen capture module"}
VIDEO_STOPPED = {"websockets": "Received STOP_VIDEO for 'primary'. Stopping stream.",
                 "webrtc": "All consumers of display 'primary' are paused; capture stopped."}
AUDIO_STARTED = "Starting pcmflux audio pipeline..."
AUDIO_STOPPED = "Stopping pcmflux audio pipeline..."
VIDEO_OFF_AT_START = {"websockets": "Display 'primary' starts with video off",
                      "webrtc": "Screen capture starts paused"}
AUDIO_OFF_AT_START = {"websockets": "Initial client settings message processed by ws_handler.",
                      "webrtc": "Audio capture starts paused"}
# How long a capture that must not start is given to prove it.
QUIET_S = 6.0


def pipe(page: Any) -> dict:
    return page.evaluate("window.__pipe") or {}


def wait_pipe(page: Any, key: str, value: bool, timeout: float = 20) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pipe(page).get(key) is value:
            return True
        time.sleep(0.25)
    return False


def post(page: Any, message: dict) -> None:
    page.evaluate("(m) => window.postMessage(m, window.location.origin)", message)


def video_up(page: Any, mode: str, timeout: float = 20) -> bool:
    return bool(C.wait_wr_video(page, timeout) if mode == "webrtc" else C.wait_ws_video(page, timeout))


def video_seen(page: Any, mode: str) -> bool:
    """Whether any video reached the page so far, without waiting."""
    if mode == "webrtc":
        return bool(page.evaluate("(() => { const v = document.querySelector('video'); return !!(v && v.videoWidth > 0); })()"))
    return bool(page.evaluate("window.videoChunksReceived > 0"))


def open_page(browser: Any, mode: str, url_hash: str = "", extra_init: Optional[list] = None) -> Any:
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, permissions=[])
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(STATUS_JS)
    for script in extra_init or []:
        ctx.add_init_script(script)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return page


def defaults_block(mode: str, wayland: bool = False) -> "H.Results":
    res = H.Results(f"start-defaults-{mode}{'-wl' if wayland else ''}")
    H.server_start(mode=mode, wayland=wayland)
    try:
        with sync_playwright() as pw:
            browser = C.launch_browser(pw)
            page = open_page(browser, mode)
            res.check("video flows", video_up(page, mode))
            res.check("audio capture started", C.wait_log(AUDIO_STARTED, timeout=15))
            time.sleep(2.0)
            state = pipe(page)
            # A state the dashboards already assume is not announced; only a
            # change is, so nothing may have been announced off.
            res.check("the page announces nothing off and no device on",
                      state.get("video") is not False and state.get("audio") is not False
                      and state.get("microphone") is not True and state.get("webcam") is not True
                      and state.get("gamepad") is not False, state)
            browser.close()
    finally:
        H.server_stop()
    return res


def video_off_block(mode: str, wayland: bool = False) -> "H.Results":
    res = H.Results(f"video-off-{mode}{'-wl' if wayland else ''}")
    H.server_start(mode=mode, wayland=wayland, extra_env={"SELKIES_VIDEO_ON_START": "false"})
    try:
        with sync_playwright() as pw:
            browser = C.launch_browser(pw)
            page = open_page(browser, mode)
            res.check("page announces video off", wait_pipe(page, "video", False), pipe(page))
            res.check("server registers the page with video off", C.wait_log(VIDEO_OFF_AT_START[mode], timeout=15))
            res.check("audio still starts", C.wait_log(AUDIO_STARTED, timeout=15))
            res.check("no screen capture starts", C.wait_log_absent(VIDEO_STARTED[mode], timeout=QUIET_S))
            res.check("no video reaches the page", not video_seen(page, mode))
            res.check("the status overlay is not left waiting for a stream",
                      page.evaluate("(() => { const s = document.getElementById('status-display'); return !s || s.classList.contains('hidden'); })()"))
            post(page, {"type": "pipelineControl", "pipeline": "video", "enabled": True})
            res.check("toggle on: the capture starts", C.wait_log(VIDEO_STARTED[mode], timeout=15))
            res.check("toggle on: video flows", video_up(page, mode, 30))
            res.check("toggle on: page announces video on", wait_pipe(page, "video", True), pipe(page))
            post(page, {"type": "pipelineControl", "pipeline": "video", "enabled": False})
            res.check("toggle off: the capture stops", C.wait_log(VIDEO_STOPPED[mode], timeout=15))
            res.check("toggle off: page announces video off", wait_pipe(page, "video", False), pipe(page))

            viewer = open_page(browser, mode, "#shared")
            res.check("a shared viewer joining the video-off session gets the stream", video_up(viewer, mode, 30))
            res.check("the owner's page stays without video", not video_seen(page, mode) or pipe(page).get("video") is False)
            viewer.context.close()
            page.context.close()
            browser.close()
    finally:
        H.server_stop()
    return res


def audio_off_block(mode: str, wayland: bool = False) -> "H.Results":
    res = H.Results(f"audio-off-{mode}{'-wl' if wayland else ''}")
    H.server_start(mode=mode, wayland=wayland, extra_env={"SELKIES_AUDIO_ON_START": "false"})
    try:
        with sync_playwright() as pw:
            browser = C.launch_browser(pw)
            page = open_page(browser, mode)
            res.check("video flows", video_up(page, mode))
            res.check("page announces audio off", wait_pipe(page, "audio", False), pipe(page))
            res.check("server sees the page connect", C.wait_log(AUDIO_OFF_AT_START[mode], timeout=15))
            res.check("no audio capture starts", C.wait_log_absent(AUDIO_STARTED, timeout=QUIET_S))
            if mode == "webrtc":
                res.check("the element carrying audio is muted",
                          page.evaluate("(() => { const v = document.querySelector('video'); return !!(v && v.muted); })()"))
            post(page, {"type": "pipelineControl", "pipeline": "audio", "enabled": True})
            res.check("toggle on: the audio capture starts", C.wait_log(AUDIO_STARTED, timeout=15))
            res.check("toggle on: page announces audio on", wait_pipe(page, "audio", True), pipe(page))
            if mode == "webrtc":
                res.check("toggle on: the element is unmuted",
                          page.evaluate("(() => { const v = document.querySelector('video'); return !!(v && !v.muted); })()"))
            post(page, {"type": "pipelineControl", "pipeline": "audio", "enabled": False})
            res.check("toggle off: the audio capture stops", C.wait_log(AUDIO_STOPPED, timeout=15))
            res.check("toggle off: page announces audio off", wait_pipe(page, "audio", False), pipe(page))

            viewer = open_page(browser, mode, "#shared")
            res.check("a shared viewer joining gets audio started for it", C.wait_log(AUDIO_STARTED, timeout=20))
            viewer.context.close()
            page.context.close()
            browser.close()
    finally:
        H.server_stop()
    return res


def microphone_block(mode: str) -> "H.Results":
    res = H.Results(f"microphone-on-{mode}")
    TM.unload_leftover_virtual_mic()
    H.server_start(mode=mode, extra_env={"SELKIES_MICROPHONE_ON_START": "true"})
    try:
        with sync_playwright() as pw:
            browser, page, errors = TM.launch(pw, mode)
            page.evaluate(STATUS_JS)
            res.check("video flows", video_up(page, mode))
            res.check("the microphone comes on without a toggle", TM.wait_status(page, True, 25),
                      str(page.evaluate("window.__micStatus")))
            res.check("the virtual microphone is provisioned",
                      C.wait_log(f"Virtual microphone '{TM.VIRTUAL_MIC}' is ready", timeout=25))
            res.check("no page errors", not errors, "; ".join(errors)[:200])
            browser.close()
    finally:
        H.server_stop()
    TM.unload_leftover_virtual_mic()
    return res


def webcam_block(mode: str) -> "H.Results":
    res = H.Results(f"webcam-on-{mode}")
    TW.build()
    cam = TW.PublishedCamera(TW.flat_frames()).start()
    H.server_start(mode=mode, extra_env={"SELKIES_WEBCAM_ENABLED": "true",
                                         "SELKIES_WEBCAM_ON_START": "true",
                                         "SELKIES_WEBCAM_PIXEL_FORMAT": "I420"})
    try:
        with sync_playwright() as pw:
            browser, page, errors = TW.launch(pw, "chromium", cam.sock_dir, mode)
            res.check("video flows", video_up(page, mode))
            res.check("the webcam comes on without a toggle", TW.wait_status(page, True, 25),
                      str(page.evaluate("window.__camStatus")))
            r = TW.wait_for_picture([((640, 360), TW.GREEN), ((20, 20), TW.BLACK)])
            res.check("the camera reaches /dev/video0", r.get("rc") == 0 and r.get("frames") == "30",
                      f"rc={r.get('rc')} frames={r.get('frames')} err={r.get('error', '')}")
            res.check("no page errors", not errors, "; ".join(errors)[:200])
            browser.close()
    finally:
        H.server_stop()
        cam.stop()
    return res


def gamepad_off_block(mode: str) -> "H.Results":
    res = H.Results(f"gamepad-off-{mode}")
    H.server_start(mode=mode, extra_env={"SELKIES_GAMEPAD_ON_START": "false"})
    try:
        with sync_playwright() as pw:
            browser = C.launch_browser(pw)
            page = open_page(browser, mode, extra_init=[P.WIRE_TAP, P.PADS_INIT])
            res.check("video flows", video_up(page, mode))
            res.check("page announces gamepad off", wait_pipe(page, "gamepad", False), pipe(page))
            time.sleep(1.0)
            page.evaluate("window.__padPress(0, 0, 1)")
            time.sleep(0.4)
            page.evaluate("window.__padPress(0, 0, 0)")
            time.sleep(0.6)
            before = P.button_reports(page)
            res.check("a pressed pad is not polled", before == 0, before)
            post(page, {"type": "gamepadControl", "enabled": True})
            time.sleep(0.3)
            page.evaluate("window.__padPress(0, 1, 1)")
            time.sleep(0.4)
            page.evaluate("window.__padPress(0, 1, 0)")
            time.sleep(0.6)
            after = P.button_reports(page)
            res.check("toggle on: button reports reach the wire", after > before, after)
            page.context.close()

            remembered = f"localStorage.setItem({P.STORAGE_APP} + '_isGamepadEnabled', 'true');"
            page = open_page(browser, mode, extra_init=[P.WIRE_TAP, P.PADS_INIT, remembered])
            res.check("remembered page: video flows", video_up(page, mode))
            time.sleep(1.0)
            page.evaluate("window.__padPress(0, 0, 1)")
            time.sleep(0.4)
            page.evaluate("window.__padPress(0, 0, 0)")
            time.sleep(0.6)
            res.check("a browser's remembered toggle wins over the policy", P.button_reports(page) > 0,
                      P.button_reports(page))
            res.check("remembered page announces gamepad on", pipe(page).get("gamepad") is not False, pipe(page))
            page.context.close()
            browser.close()
    finally:
        H.server_stop()
    return res


def main() -> int:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    selectors = ["websockets", "webrtc", "websockets-wl", "webrtc-wl"] if which == "all" else [which]
    blocks = []
    for selector in selectors:
        mode, wayland = selector.replace("-wl", ""), selector.endswith("-wl")
        blocks.append(defaults_block(mode, wayland))
        blocks.append(video_off_block(mode, wayland))
        blocks.append(audio_off_block(mode, wayland))
        if not wayland:
            for block in (microphone_block, webcam_block, gamepad_off_block):
                blocks.append(block(mode))
    summaries = [b.summary() for b in blocks]
    return 0 if all(summaries) else 1


if __name__ == "__main__":
    sys.exit(main())
