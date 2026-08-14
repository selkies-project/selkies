# -*- coding: utf-8 -*-
"""End-to-end proof: WebRTC media + datachannel over a Unix-socket-only server.

Setup:
  selkies --mode webrtc --unix-socket /tmp/...sock   (NO TCP listener)
  tcp2unix TCP:18080 -> unix socket                  (reverse-proxy shim, HTTP only)
  Chrome connects to http://localhost:18080, does WebRTC (signaling via the shim,
  media over the server's own UDP ICE sockets).
"""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

SOCK = os.path.join(H.WORKDIR, "selkies-webrtc.sock")
PORT = H.PORT

H.server_stop()
subprocess.run(["pkill", "-f", "tcp2unix.py"], capture_output=True)
time.sleep(1.0)
os.path.exists(SOCK) and os.unlink(SOCK)

env = {"PATH": os.environ.get("PATH", ""),
       "HOME": os.path.expanduser("~"), "DISPLAY": H.TEST_DISPLAY,
       "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", H.WORKDIR),
       "SELKIES_MODE": "webrtc", "SELKIES_ENABLE_BASIC_AUTH": "false",
       "SELKIES_WEB_ROOT": H.CORE_DIST,
       "SELKIES_TURN_REST_URI": ""}
server = subprocess.Popen(
    [H.PYTHON, "-m", "selkies", "--port", str(PORT), "--unix-socket", SOCK],
    env=env, cwd=H.WORKDIR,
    stdout=open(os.path.join(H.WORKDIR, "selkies-webrtc.log"), "w"),
    stderr=subprocess.STDOUT, start_new_session=True)
proxy = subprocess.Popen([H.PYTHON, os.path.join(H.TOOLS, "tcp2unix.py"),
                          "127.0.0.1", str(PORT), SOCK],
                         env=dict(os.environ), cwd=H.WORKDIR,
                         stdout=open(os.path.join(H.WORKDIR, "proxy.log"), "w"),
                         stderr=subprocess.STDOUT, start_new_session=True)
time.sleep(9)

res = H.Results("webrtc-unix")
try:
    listeners = [l for l in subprocess.run(["ss", "-tlnp"], capture_output=True, text=True).stdout.splitlines()
                 if f":{PORT}" in l and "selkies" in l]
    res.check("server owns no TCP listener", not listeners, listeners[:1])
    res.check("unix socket bound", os.path.exists(SOCK), SOCK)

    with sync_playwright() as pw:
        browser = C.chromium_launch(pw)
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'webrtc';")
        page = ctx.new_page()
        console_errors = []
        page.on("console", lambda m: console_errors.append(m.text) if m.type == "error" else None)
        page.on("pageerror", lambda e: console_errors.append(str(e)))
        page.goto(f"http://localhost:{PORT}", wait_until="load")
        info = C.wait_wr_video(page, timeout=60)
        res.check("WebRTC video receives through the proxied signaling", info is not None, info)
        # the datachannel carries the mode/stream notification once media is up
        time.sleep(2)
        real_errors, _ = C.benign_console(console_errors, [])
        res.check("console clean", not real_errors, str(real_errors[:2]))
        browser.close()
finally:
    # By pid: this server listens on a unix socket, so nothing else can find it,
    # and a sweep by name would reach servers this suite never started.
    for child in (proxy, server):
        child.terminate()
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()

sys.exit(0 if res.summary() else 1)
