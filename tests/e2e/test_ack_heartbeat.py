#!/usr/bin/env python3
"""The client's ack heartbeat on a still screen, in each engine, on both ack
paths (turbo off).

A damage-gated capture sends nothing while the screen is still, and the
server reads a frame left unanswered for seconds as a stalled client, so the
client repeats its last ack about once a second: that is what tells an idle
client from a dead one. The socket worker sends those acks, which a page-side
hook never sees, so the page reaches the server through `tools/ws_tap.py`
and the acks are counted on the wire. Full frames (h264enc) ack on receipt
and the striped modes (jpeg) ack what the video worker presented; both are
covered, and after the still period the stream must resume with the first
change on the screen. WebKit decodes full-frame H.264 only after the client's
software-decode retry, so that case is seeded with the persisted preference
rather than driven through the fallback ladder's reload.

Uses `E2E_DISPLAY` when set; otherwise starts a throwaway Xvfb.
Usage: python3 tests/e2e/test_ack_heartbeat.py [chromium|firefox|webkit|all]
"""
import os
import socket
import statistics
import subprocess
import sys
import time
from typing import Any, List, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

TAP = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "tools", "ws_tap.py")
TAP_LOG = os.path.join(H.WORKDIR, "ws-tap.log")
ENGINES = ("chromium", "firefox", "webkit")
# The client's persisted software-decode preference, keyed by the browser build.
SOFTWARE_DECODE_JS = ("try { localStorage.setItem((location.origin + location.pathname)"
                      ".replace(/[^a-zA-Z0-9._-]/g, '_') + '_prefer_software_decode', navigator.userAgent); }"
                      " catch (e) {}")


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def acks_since(mark: int) -> Tuple[List[Tuple[float, str]], int, int]:
    """The frame acks the tap logged after byte `mark`, as (time, frame id),
    how many times the page connected anew meanwhile (a reload), and how
    many keyframes it asked for."""
    out = []
    reloads = requests = 0
    with open(TAP_LOG) as f:
        f.seek(mark)
        for line in f:
            t, _, msg = line.rstrip("\n").partition(" ")
            if msg.startswith("CLIENT_FRAME_ACK"):
                out.append((float(t), msg.split()[1]))
            elif msg.startswith("SETTINGS,"):
                reloads += 1
            elif msg.startswith("REQUEST_KEYFRAME"):
                requests += 1
    return out, reloads, requests


def open_page(pw: Any, engine: str, port: int, software: bool) -> Tuple[Any, Any]:
    """Returns what to close and the page; Firefox runs on the persistent profile."""
    if engine == "firefox":
        ctx = C.firefox_persistent_context(pw, viewport={"width": 1280, "height": 720})
        closer = ctx
    else:
        browser = C.launch_browser(pw, engine)
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        closer = browser
    ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
    if software:
        ctx.add_init_script(SOFTWARE_DECODE_JS)
    page = ctx.new_page()
    page.goto(f"http://localhost:{port}/", wait_until="load")
    return closer, page


def drive(res: H.Results, pw: Any, engine: str, encoder: str, port: int) -> None:
    tag = f"[{engine}/{encoder}]"
    if engine == "firefox" and encoder == "h264enc" and not C.openh264_version():
        res.skip(f"{tag} no OpenH264 GMP plugin in {C.FF_E2E_PROFILE}",
                 "run tests/tools/fetch-openh264.sh to cover H.264 in Firefox")
        return
    churn = C.Churn()
    churn.start()
    try:
        closer, page = open_page(pw, engine, port, engine == "webkit" and encoder == "h264enc")
    except Exception as e:
        res.skip(f"{tag} engine unavailable", str(e)[:120])
        churn.stop()
        return
    try:
        res.check(f"{tag} video flows", bool(C.wait_ws_video(page, timeout=60)), "")
        motion_mark = os.path.getsize(TAP_LOG)
        time.sleep(3)
        divert = page.evaluate("!!window.videoDivertOn")
        # A decoder that keeps up acks a stream of new ids; one that does not
        # (Playwright's WebKit on full-frame H.264) leaves frames unacked, and
        # only the heartbeat and the stall verdict mean anything there.
        moving = len({a[1] for a in acks_since(motion_mark)[0]})
        decodes = moving >= 40
        server_mark = os.path.getsize(H.LOG)
        churn.stop()
        # The paint-over burst that follows the last change has to be over.
        time.sleep(2.5)
        mark = os.path.getsize(TAP_LOG)
        chunks0 = page.evaluate("window.videoChunksReceived")
        time.sleep(8)
        acks, reloads, requests = acks_since(mark)
        chunks1 = page.evaluate("window.videoChunksReceived")
        ids = {a[1] for a in acks}
        gaps = [b[0] - a[0] for a, b in zip(acks, acks[1:])]
        median = statistics.median(gaps) if gaps else None
        # A decoder that keeps asking for keyframes is sent them; anything
        # beyond those is the capture sending on its own.
        if decodes:
            res.check(f"{tag} the still screen sends nothing unasked and the page stays up",
                      reloads == 0 and 0 <= chunks1 - chunks0 <= requests + 1,
                      f"{chunks1 - chunks0} chunks in 8 s, {requests} keyframe requests, {reloads} reloads")
        else:
            res.skip(f"{tag} the still screen sends nothing unasked",
                     f"the decoder acked {moving} distinct ids in 3 s of motion, so what it is sent says nothing")
        res.check(f"{tag} an unchanged id is re-acked about once a second (worker divert={divert})",
                  6 <= len(acks) <= 10 and len(ids) <= 2 and median is not None and 0.8 <= median <= 1.3,
                  f"{len(acks)} acks, ids={sorted(ids)}, median gap={median and round(median, 2)} s")
        with open(H.LOG, "rb") as f:
            f.seek(server_mark)
            stalls = f.read().decode("utf-8", "replace").count("Client stall for")
        res.check(f"{tag} the server declares no stall", stalls == 0, f"{stalls} stalls")
        if not decodes:
            res.skip(f"{tag} the first change on the screen resumes the stream", "same decoder")
            return
        churn.start()
        t0 = time.time()
        first = None
        while time.time() - t0 < 5:
            if page.evaluate("window.videoChunksReceived") > chunks1 + 2:
                first = time.time() - t0
                break
            time.sleep(0.1)
        res.check(f"{tag} the first change on the screen resumes the stream",
                  first is not None and first < 2.0, f"after {first and round(first, 2)} s")
    finally:
        closer.close()
        churn.stop()


def main(selection: str) -> H.Results:
    res = H.Results("ack-heartbeat")
    engines = ENGINES if selection == "all" else (selection,)
    xproc = None
    if not H.TEST_DISPLAY:
        xproc, H.TEST_DISPLAY = H.private_x_server()
    port = free_port()
    open(TAP_LOG, "w").close()
    tap = H.spawn([sys.executable, TAP, str(port), str(H.PORT), TAP_LOG],
                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for encoder in ("jpeg", "h264enc"):
            H.server_start(mode="websockets", wayland=False, extra_env={
                "SELKIES_USE_CPU": "true", "SELKIES_VIDEO_STREAMING_MODE": "false",
                "SELKIES_ENCODER": encoder})
            with sync_playwright() as pw:
                for engine in engines:
                    drive(res, pw, engine, encoder, port)
    finally:
        tap.terminate()
        H.server_stop()
        if xproc is not None:
            H.stop_x_server(xproc, H.TEST_DISPLAY)
    res.summary()
    return res


if __name__ == "__main__":
    r = main(sys.argv[1] if len(sys.argv) > 1 else "all")
    sys.exit(0 if not r.failed() else 1)
