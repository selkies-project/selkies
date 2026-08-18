#!/usr/bin/env python3
"""Kernel gamepad end-to-end: a real browser client with a synthetic Gamepad API
drives selkies over each transport; the /dev/uinput emulator records what the
kernel would receive."""
import os
import struct
import sys
import time
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright
sys.path.insert(0, H.SRC)
import selkies.input_handler as ih


PAD_INIT: str = """
window.__pad = {
  index: 0, id: "Selkies Test Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)",
  mapping: "standard", connected: true, timestamp: 1,
  buttons: Array.from({length: 17}, () => ({pressed: false, touched: false, value: 0})),
  axes: [0, 0, 0, 0],
};
navigator.getGamepads = () => [window.__pad, null, null, null];
window.__padPress = (i, v) => {
  window.__pad.buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
  window.__pad.timestamp = performance.now();
};
window.__padAxis = (i, v) => {
  window.__pad.axes[i] = v;
  window.__pad.timestamp = performance.now();
};
"""

def decode(path: str) -> list[tuple[int, int, int]]:
    """Decode a uinput-shim event stream into (type, code, value) tuples.

    Args:
        path: Path to the shim's binary event stream file.

    Returns:
        One `(ev_type, ev_code, ev_value)` tuple per 24-byte input_event
        record, timestamps stripped.
    """
    blob = open(path, "rb").read()
    return [struct.unpack("=qqHHi", blob[o:o + 24])[2:] for o in range(0, len(blob) - 23, 24)]

def launch(pw, mode: str):
    """Launch Chromium with the synthetic pad injected and open the stream page.

    Args:
        pw: Active Playwright instance.
        mode: Transport mode, ``websockets`` or ``webrtc``.

    Returns:
        Tuple of (browser, page, console-error list).
    """
    browser = C.chromium_launch(pw)
    ctx = browser.new_context(viewport={"width": 1280, "height": 720})
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(PAD_INIT)
    page = ctx.new_page()
    errors = []
    page.on("console", lambda m: errors.append(m.text) if m.type == "error" else None)
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(H.BASE_URL + "/", wait_until="load")
    return browser, page, errors

def run(mode: str, results: "H.Results") -> None:
    """Drive pad input over one transport and verify the kernel-side record.

    Args:
        mode: Transport mode, ``websockets`` or ``webrtc``.
        results: Results accumulator shared across both transports.
    """
    shim_env, STREAM, SHIMLOG = H.uinput_shim_env(f"e2e-{mode}")
    H.server_start(mode=mode, extra_env=shim_env)
    try:
        with sync_playwright() as pw:
            browser, page, errors = launch(pw, mode)
            video = C.wait_wr_video(page) if mode == "webrtc" else C.wait_ws_video(page)
            results.check(f"{mode}: video flowing", bool(video), str(video))
            for action in ("__padPress(0, 1)", "__padPress(0, 0)",
                           "__padPress(12, 1)", "__padPress(12, 0)",
                           "__padAxis(0, -1)", "__padPress(6, 1)"):
                page.evaluate(f"window.{action}")
                time.sleep(0.25)
            time.sleep(0.5)
            real_errors, _ = C.benign_console(errors, [])
            results.check(f"{mode}: console clean", not real_errors, str(real_errors[:2]))
            browser.close()
    finally:
        H.server_stop()

    log = open(SHIMLOG).read()
    events = decode(STREAM)
    results.check(f"{mode}: kernel device created", log.count("DEV_CREATE") == 1)
    results.check(f"{mode}: device is the standard pad",
                  "vendor=0x045e product=0x028e" in log and "Microsoft X-Box 360 pad" in log)
    for name, event in (("A press", (ih.EV_KEY, ih.BTN_A, 1)),
                        ("A release", (ih.EV_KEY, ih.BTN_A, 0)),
                        ("dpad up", (ih.EV_ABS, ih.ABS_HAT0Y, -1)),
                        ("dpad release", (ih.EV_ABS, ih.ABS_HAT0Y, 0)),
                        ("stick left", (ih.EV_ABS, ih.ABS_X, -32767)),
                        ("left trigger", (ih.EV_ABS, ih.ABS_Z, 32767))):
        results.check(f"{mode}: {name} reached the kernel device", event in events)
    print(f"    {mode} events:", [e for e in events if e[0] != 4])
    synced = all(events[i + 1] == (ih.EV_SYN, 0, 0)
                 for i in range(0, len(events) - 1, 2)) and len(events) % 2 == 0
    results.check(f"{mode}: every event is framed by SYN_REPORT", synced, f"{len(events)} events")

results = H.Results("uinput")
for mode in ("websockets", "webrtc"):
    run(mode, results)
sys.exit(0 if results.summary() else 1)
