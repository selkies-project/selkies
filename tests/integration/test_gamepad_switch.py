"""A kernel gamepad must survive a transport mode switch: applications hold the
device open, so it may not be recreated (or lost) when the service restarts."""
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

PAD = H.pad_init_js()

def decode(path):
    blob = open(path, "rb").read()
    return [struct.unpack("=qqHHi", blob[o:o + 24])[2:] for o in range(0, len(blob) - 23, 24)]

def press(pw, mode, res, tag):
    browser = C.chromium_launch(pw)
    ctx = browser.new_context(viewport={"width": 1280, "height": 720})
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(PAD)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    res.check(f"{tag}: video flowing", bool((C.wait_wr_video if mode == "webrtc" else C.wait_ws_video)(page)))
    before = len(decode(STREAM))
    page.evaluate("window.__padPress(3, 1)")
    time.sleep(0.4)
    page.evaluate("window.__padPress(3, 0)")
    time.sleep(0.4)
    events = decode(STREAM)[before:]
    res.check(f"{tag}: input reaches the kernel device", (ih.EV_KEY, ih.BTN_Y, 1) in events, str(events[:2]))
    browser.close()

res = H.Results("switch")
shim_env, STREAM, SHIMLOG = H.uinput_shim_env("switch")
H.server_start(mode="websockets", extra_env=shim_env)
try:
    with sync_playwright() as pw:
        press(pw, "websockets", res, "websockets")
        status, _ = H.curl("/api/switch", method="POST", data={"mode": "webrtc"})
        res.check("switch accepted", status == 200, status)
        time.sleep(3)
        press(pw, "webrtc", res, "after switch")
    log = open(SHIMLOG).read()
    res.check("kernel device created once", log.count("DEV_CREATE") == 1, f"{log.count('DEV_CREATE')} creations")
    res.check("kernel device never destroyed by the switch", log.count("DEV_DESTROY") == 0)
finally:
    H.server_stop()
sys.exit(0 if res.summary() else 1)
