"""A pad left mid-press when the client goes away must be neutralized on both
transports (reset_state via the peer/connection-gone path)."""
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

res = H.Results("release")
for mode in ("websockets", "webrtc"):
    shim_env, STREAM, _ = H.uinput_shim_env(f"release-{mode}")
    H.server_start(mode=mode, extra_env=shim_env)
    try:
        with sync_playwright() as pw:
            browser = C.chromium_launch(pw)
            ctx = browser.new_context(viewport={"width": 1280, "height": 720})
            ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            ctx.add_init_script(PAD)
            page = ctx.new_page()
            page.goto(H.BASE_URL + "/", wait_until="load")
            (C.wait_wr_video if mode == "webrtc" else C.wait_ws_video)(page)
            page.evaluate("window.__padPress(0, 1)")   # hold A
            time.sleep(0.4)
            held = decode(STREAM)
            res.check(f"{mode}: button held", (ih.EV_KEY, ih.BTN_A, 1) in held)
            browser.close()
            deadline = time.time() + 30
            while time.time() < deadline:
                if (ih.EV_KEY, ih.BTN_A, 0) in decode(STREAM):
                    break
                time.sleep(1)
            waited = round(time.time() - (deadline - 30), 1)
            res.check(f"{mode}: held button released when the client disappears",
                      (ih.EV_KEY, ih.BTN_A, 0) in decode(STREAM), f"after {waited}s")
    finally:
        H.server_stop()
sys.exit(0 if res.summary() else 1)
