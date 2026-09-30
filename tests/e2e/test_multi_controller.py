#!/usr/bin/env python3
"""Two controllers on one display: a page of another tab joins the display's
stream beside its owner instead of taking it over, on both transports. Both
stream and both drive input; the owner keeps sizing the display, a reload of
the owner's own page takes its connection back without touching the other, and
once the owner is gone for good the page beside it owns the display and sets
its size."""
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


def main() -> int:
    res = H.Results("multi-controller")
    for mode in ("websockets", "webrtc"):
        run(res, mode)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
