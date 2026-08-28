#!/usr/bin/env python3
"""Pointer lock against the server: the remote pointer travels by the deltas.

A locked page gets no positions from the browser, only movementX/Y, and the
client turns those into `m2,dx,dy` messages the server injects as relative
motion (XTEST on X11, the compositor's relative pointer on Wayland). Here a
real pointer lock is taken in the browser (the client's Ctrl+Shift+click on
the stream) and every move the test makes while it holds must move the server
pointer by exactly that much — a single move, a run of small ones, and a
negative one — and releasing the lock must put the client back on absolute
positions. The wire is tapped at WebSocket/RTCDataChannel send so the
messages that carried the motion are checked as well as the pointer; the
pointer itself is read from the X server or, on Wayland, from the observer
surface the compositor delivers pointer events to. Locked deltas are scaled
by the client from CSS pixels to stream pixels, so the expectations follow
the stream size the server realized; that the stream matches the window at
connect is its own check.

Headless Chromium's full build is driven (not the headless shell, whose
locked movement deltas do not add up), on both transports and backends.

    python3 tests/e2e/test_pointer_lock.py ws-x11|wr-x11|ws-wl|wr-wl
"""
import os
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

WL_SOCKET = "wayland-1"
START = (640, 360)
# (dx, dy, repeat): one plain move, a run of small ones, and a move back.
MOVES = ((60, 40, 1), (5, -3, 10), (-200, 100, 1))

# Motion messages off the shared wire tap, whichever thread owns the socket.
MOVES_JS = ("window.__wireSent.filter(d => typeof d === 'string' && "
            "(d.startsWith('m,') || d.startsWith('m2,')))")


def launch(p: Any, mode: str) -> tuple:
    """The full Chromium build on the stream page, with the wire tap installed."""
    kw = {"headless": True, "args": C.BROWSER_ARGS}
    if C.CHROME_PATH:
        kw["executable_path"] = C.CHROME_PATH
    else:
        kw["channel"] = "chromium"
    browser = p.chromium.launch(**kw)
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(C.WIRE_TAP_JS)
    page = ctx.new_page()
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(H.BASE_URL + "/", wait_until="load")
    return browser, page, errors


class Pointer:
    """Where the server's pointer is, read from the X server or the Wayland
    observer, polled until it reaches an expected spot or time runs out."""

    def __init__(self, obs: Optional["H.WlObs"]) -> None:
        self.obs = obs

    def read(self) -> Optional[tuple]:
        if self.obs is None:
            return C.x11_mouse_pos()
        seen = [(line["x"], line["y"]) for line in self.obs.lines
                if line.get("kind") in ("ptr_enter", "ptr_motion")]
        return seen[-1] if seen else None

    def wait(self, expected: tuple, timeout: float = 6) -> Optional[tuple]:
        deadline = time.time() + timeout
        pos = self.read()
        while time.time() < deadline:
            pos = self.read()
            if pos is not None and tuple(int(round(v)) for v in pos) == expected:
                return pos
            time.sleep(0.1)
        return pos


def at(pos: Optional[tuple], expected: tuple) -> bool:
    return pos is not None and tuple(int(round(v)) for v in pos) == expected


def wait_stream_size(page: Any, mode: str, size: tuple, timeout: float = 8) -> Optional[dict]:
    """The stream's dimensions, polled until they match `size` or time runs out."""
    deadline = time.time() + timeout
    info = None
    while time.time() < deadline:
        info = C.wait_ws_video(page, timeout=2) if mode == "websockets" else C.wait_wr_video(page, timeout=2)
        if info and (info["w"], info["h"]) == size:
            return info
        time.sleep(0.5)
    return info


def moves_since(page: Any, start: int) -> list:
    return page.evaluate(MOVES_JS)[start:]


def relative_sum(messages: list) -> tuple:
    dx = dy = 0
    for m in messages:
        if m.startswith("m2,"):
            parts = m.split(",")
            dx += int(parts[1])
            dy += int(parts[2])
    return dx, dy


def run(mode: str, wayland: bool, res: "H.Results") -> None:
    H.server_start(mode=mode, wayland=wayland)
    obs = None
    try:
        if wayland:
            obs = H.WlObs(WL_SOCKET)
            res.check("wl observer mapped", obs.ready(20))
        pointer = Pointer(obs)
        with sync_playwright() as p:
            browser, page, errors = launch(p, mode)
            try:
                video = C.wait_ws_video(page, timeout=30) if mode == "websockets" else C.wait_wr_video(page)
                res.check("video flowing", bool(video), video)
                video = wait_stream_size(page, mode, (1280, 720)) or video
                res.check("stream follows the 1280x720 window at connect",
                          video and (video["w"], video["h"]) == (1280, 720), video)
                # Locked deltas and positions are scaled by the client onto the
                # stream the server realized; a stream that did not follow the
                # window still has to move the pointer by exactly what it scaled to.
                scale = (video["w"] / 1280.0) if video else 1.0

                def server(css: tuple) -> tuple:
                    return (int(round(css[0] * scale)), int(round(css[1] * scale)))

                time.sleep(1.0)
                page.mouse.move(*START)
                res.check("absolute move lands the pointer at the start",
                          at(pointer.wait(server(START)), server(START)), pointer.read())

                page.keyboard.down("Control")
                page.keyboard.down("Shift")
                page.mouse.click(*START)
                page.keyboard.up("Shift")
                page.keyboard.up("Control")
                deadline = time.time() + 5
                locked = False
                while time.time() < deadline and not locked:
                    locked = page.evaluate("document.pointerLockElement !== null")
                    time.sleep(0.1)
                res.check("Ctrl+Shift+click takes the pointer lock", locked)
                time.sleep(0.5)

                x, y = START
                cursor = (x, y)
                for dx, dy, repeat in MOVES:
                    mark = len(page.evaluate(MOVES_JS))
                    for _ in range(repeat):
                        cursor = (cursor[0] + dx, cursor[1] + dy)
                        page.mouse.move(*cursor)
                        time.sleep(0.05)
                    x, y = x + dx * repeat, y + dy * repeat
                    pos = pointer.wait(server((x, y)))
                    label = f"{repeat} x ({dx},{dy})" if repeat > 1 else f"({dx},{dy})"
                    res.check(f"locked move {label} carries the server pointer to {server((x, y))}",
                              at(pos, server((x, y))), pos)
                    sent = moves_since(page, mark)
                    res.check(f"locked move {label} went out as relative motion",
                              sent and all(m.startswith("m2,") for m in sent), sent[:4])
                    res.check(f"locked move {label} deltas add up on the wire",
                              relative_sum(sent) == server((dx * repeat, dy * repeat)), relative_sum(sent))

                mark = len(page.evaluate(MOVES_JS))
                page.evaluate("document.exitPointerLock()")
                deadline = time.time() + 5
                while time.time() < deadline and page.evaluate("document.pointerLockElement !== null"):
                    time.sleep(0.1)
                res.check("lock released", page.evaluate("document.pointerLockElement === null"))
                time.sleep(0.3)
                target = (300, 200)
                page.mouse.move(*target)
                pos = pointer.wait(server(target))
                res.check("after release an absolute move lands where it says", at(pos, server(target)), pos)
                sent = moves_since(page, mark)
                res.check("after release the motion goes out as positions",
                          sent and sent[-1].startswith("m,"), sent[-2:])
                res.check("no page errors", not errors, "; ".join(errors)[:200])
            finally:
                browser.close()
    finally:
        if obs is not None:
            obs.stop()
        H.server_stop()


SELECTORS = ("ws-x11", "wr-x11", "ws-wl", "wr-wl")


def main() -> bool:
    which = sys.argv[1] if len(sys.argv) > 1 else "ws-x11"
    if which not in SELECTORS:
        raise SystemExit(f"unknown selector {which!r}; one of {SELECTORS}")
    transport, backend = which.split("-")
    res = H.Results(f"pointer-lock-{which}")
    run("websockets" if transport == "ws" else "webrtc", backend == "wl", res)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
