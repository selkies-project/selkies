#!/usr/bin/env python3
"""Integration checks for the current working tree.

Covers the X11/Wayland x WebSockets/WebRTC matrix end to end plus the three
regressions the audit turned up: Prometheus collector re-registration on a mode
switch, the WebRTC pacer draining large packets, and multipart clipboard
reassembly after a client cleanup.

Usage: python test_ci_fixes.py [matrix|switch|clipboard|pacer|all]
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C


def _log_has(pattern, log=H.LOG):
    return re.search(pattern, H.server_log(log) or "", re.I) is not None


def _wait_clip(page, text, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if C.server_clipboard_push_check(page, text):
            return True
        time.sleep(0.5)
    return False


def block_matrix(r):
    """Every backend x transport combination streams video to a real browser."""
    from playwright.sync_api import sync_playwright
    for wayland in (False, True):
        for mode in ("websockets", "webrtc"):
            name = ("wayland" if wayland else "x11") + "/" + mode
            try:
                H.server_start(mode=mode, wayland=wayland)
            except Exception as e:
                r.check(f"{name} server up", False, e)
                continue
            r.check(f"{name} server up", True)
            try:
                with sync_playwright() as pw:
                    browser, page, errs, nf = C.launch_chrome(pw, mode=mode)
                    try:
                        ok = (C.wait_wr_video(page) if mode == "webrtc"
                              else C.wait_ws_video(page))
                        r.check(f"{name} video flowing", ok, ok)
                        real, bad404 = C.benign_console(errs, nf)
                        r.check(f"{name} console clean", not real and not bad404,
                                (real[:2], bad404[:2]))
                    finally:
                        browser.close()
            except Exception as e:
                r.check(f"{name} client", False, e)
            finally:
                H.server_stop()


def block_switch(r):
    """Switching transports twice must not trip DuplicateTimeseries.

    Metrics.unregister() has to release every collector it registered, or the
    second entry into a mode raises at Metrics() construction and the service
    dies before it can serve.
    """
    H.server_start(mode="websockets",
                   extra_env={"SELKIES_ENABLE_METRICS_HTTP": "true"})
    try:
        # The Prometheus registry is served from the streaming port; there is no
        # second listener to point a scraper at.
        try:
            st, body = H.curl("/api/metrics")
        except Exception as e:
            st, body = 0, str(e).encode()
        r.check("metrics served on the streaming port",
                st == 200 and b"fps" in body, f"{st} {body[:80]}")
        for target in ("webrtc", "websockets", "webrtc"):
            try:
                st, _ = H.curl("/api/switch", method="POST", data={"mode": target})
            except Exception as e:
                r.check(f"switch -> {target} accepted", False, e)
                break
            r.check(f"switch -> {target} accepted", st in (200, 202), st)
            deadline = time.time() + 30
            got = None
            while time.time() < deadline:
                try:
                    st, body = H.curl("/api/status")
                    if st == 200 and target in body.decode(errors="replace"):
                        got = target
                        break
                except Exception:
                    pass
                time.sleep(0.5)
            r.check(f"mode is {target} after switch", got == target,
                    H.server_log(tail=4))
        r.check("no DuplicateTimeseries", not _log_has(r"Duplicated timeseries"),
                "\n".join(l for l in (H.server_log() or "").splitlines()
                          if "Duplicated" in l)[:200])
        r.check("no unexpected service termination",
                not _log_has(r"Service terminated unexpectedly"))
    finally:
        H.server_stop()


def block_clipboard(r):
    """Server->client clipboard survives a client cleanup/reconnect cycle.

    The multipart assembler is built by a factory; a cleanup that replaces the
    object with a bare literal strips its methods and every later multipart
    transfer is dropped.
    """
    from playwright.sync_api import sync_playwright
    H.server_start(mode="websockets")
    try:
        payload = ("selkies-clip-" + "y" * 200000).encode()
        ext, stop = H.x_own_clipboard(payload)
        try:
            with sync_playwright() as pw:
                browser, page, errs, nf = C.launch_chrome(pw, mode="websockets")
                try:
                    r.check("first connect streams", bool(C.wait_ws_video(page)))
                    r.check("multipart clipboard arrives",
                            _wait_clip(page, payload.decode()))
                    page.reload(wait_until="load")
                    r.check("reconnect streams", bool(C.wait_ws_video(page)))
                    r.check("multipart clipboard arrives after reconnect",
                            _wait_clip(page, payload.decode()))
                finally:
                    browser.close()
        finally:
            stop["flag"] = True
    finally:
        H.server_stop()


def block_pacer(r):
    """The WebRTC pacer must attach and keep draining, not wedge the stream."""
    from playwright.sync_api import sync_playwright
    H.server_start(mode="webrtc", extra_env={"SELKIES_WEBRTC_PACER": "true"})
    try:
        with sync_playwright() as pw:
            browser, page, errs, nf = C.launch_chrome(pw, mode="webrtc")
            try:
                r.check("webrtc video flowing", bool(C.wait_wr_video(page)))
                r.check("pacer enabled", C.wait_log("pacer enabled", timeout=25),
                        H.server_log(tail=3))
                # A wedged pacer stops the stream: frames must still climb after
                # the queue has had time to fill.
                time.sleep(6)
                fps = C.page_fps(page)
                r.check("stream still alive with pacer", bool(fps), fps)
                r.check("no pacer queue overflow purge storm",
                        not _log_has(r"pacer.*purge.*purge.*purge"))
            finally:
                browser.close()
    finally:
        H.server_stop()


BLOCKS = {"matrix": block_matrix, "switch": block_switch,
          "clipboard": block_clipboard, "pacer": block_pacer}

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    names = list(BLOCKS) if which == "all" else [which]
    ok = True
    for n in names:
        print(f"=== {n} ===", flush=True)
        res = H.Results(n)
        try:
            BLOCKS[n](res)
        except Exception as e:
            res.check("block completed", False, e)
        ok = res.summary() and ok
    sys.exit(0 if ok else 1)
