"""A pad left mid-press when the client goes away must be neutralized on both
transports: on a graceful close via the peer/connection-gone path
(reset_state), and on an ungraceful client death — where no close arrives on
any layer — via the held-state heartbeat sweep."""
import os
import struct
import sys
import time
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
import psutil
from playwright.sync_api import sync_playwright
sys.path.insert(0, H.SRC)
import selkies.input_handler as ih

PAD: str = H.pad_init_js()

def decode(path: str) -> list:
    """Decode a uinput-shim event stream into (type, code, value) tuples.

    Args:
        path: Path to the shim's binary event stream file.

    Returns:
        One `(ev_type, ev_code, ev_value)` tuple per 24-byte input_event
        record, timestamps stripped.
    """
    blob = open(path, "rb").read()
    return [struct.unpack("=qqHHi", blob[o:o + 24])[2:] for o in range(0, len(blob) - 23, 24)]

def kill_browser() -> None:
    """SIGKILL every browser process under this test: a client machine dying
    mid-press sends no close on any layer (no TCP FIN, no SCTP teardown), so
    release must come from the input-liveness sweep, not transport teardown."""
    for proc in psutil.Process().children(recursive=True):
        try:
            name = proc.name().lower()
            if "chrom" in name or "headless" in name:
                proc.kill()
        except psutil.Error:
            pass

res = H.Results("release")
for mode in ("websockets", "webrtc"):
    for death in ("close", "kill"):
        shim_env, STREAM, _ = H.uinput_shim_env(f"release-{mode}-{death}")
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
                # Hold button A (index 0) without releasing it. The press rides
                # the input channel, which opens after the video on WebRTC, so
                # it is re-sent until the kernel device shows it held.
                deadline = time.time() + 15
                while time.time() < deadline:
                    page.evaluate("window.__padPress(0, 1)")
                    time.sleep(0.4)
                    if (ih.EV_KEY, ih.BTN_A, 1) in decode(STREAM):
                        break
                held = decode(STREAM)
                tag = "" if death == "close" else " before kill"
                res.check(f"{mode}: button held{tag}", (ih.EV_KEY, ih.BTN_A, 1) in held)
                if death == "close":
                    browser.close()
                else:
                    kill_browser()
                    try:
                        browser.close()
                    except Exception:
                        pass
                deadline = time.time() + 30
                while time.time() < deadline:
                    if (ih.EV_KEY, ih.BTN_A, 0) in decode(STREAM):
                        break
                    time.sleep(1)
                waited = round(time.time() - (deadline - 30), 1)
                label = ("held button released when the client disappears"
                         if death == "close" else
                         "held button released on ungraceful kill")
                res.check(f"{mode}: {label}",
                          (ih.EV_KEY, ih.BTN_A, 0) in decode(STREAM), f"after {waited}s")
        finally:
            H.server_stop()
sys.exit(0 if res.summary() else 1)
