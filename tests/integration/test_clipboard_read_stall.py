#!/usr/bin/env python3
"""The clipboard read-back must not hold the messages behind it.

`cr` is the first thing a client sends on a new connection, ahead of the
messages that bring the session to the size and density its page asked for. An
X selection whose owner answers no conversion request -- a browser being torn
down still owns CLIPBOARD -- is read to the reader's own bound, tens of
seconds, so the read is answered off the dispatch loop and the session comes up
while it is still outstanding.

The dispatcher is the one both transports share, so the websockets wire covers
the WebRTC data channel with it.

Usage: python3 tests/integration/test_clipboard_read_stall.py
"""
import asyncio
import base64
import json
import os
import sys
import time
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import websockets  # noqa: E402

INITIAL_W, INITIAL_H = 1280, 720
WANT_W, WANT_H = 1600, 900
PAYLOAD = b"read-back served by an owner that answers"


def settings_payload() -> str:
    """The initial SETTINGS a primary page sends, sizing the desktop to it."""
    return "SETTINGS," + json.dumps({
        "displayId": "primary",
        "initialClientWidth": INITIAL_W, "initialClientHeight": INITIAL_H,
        "manual_resolution": False, "framerate": 60, "encoder": "h264enc",
        "scaling_dpi": 96,
    })


def silent_owner() -> Any:
    """Own CLIPBOARD on the test display and answer no conversion request.

    Returns:
        The connection; closing it releases the selection.
    """
    from selkies.Xlib import display as xdisp, X
    d = xdisp.Display(H.require_display())
    scr = d.screen()
    win = scr.root.create_window(0, 0, 1, 1, 0, scr.root_depth,
                                 window_class=X.InputOutput)
    win.set_selection_owner(d.get_atom("CLIPBOARD"), X.CurrentTime)
    d.flush()
    return d


async def wait_root(w: int, h: int, timeout: float) -> tuple:
    """Poll the X root until it is within the CVT cell of `w`x`h`.

    Polled on the loop rather than blocking it: the socket this waits on has to
    keep answering keepalives while the desktop follows.

    Returns:
        The last size read, for the caller to compare and report.
    """
    deadline = time.time() + timeout
    realized = H.x_root_size()
    while time.time() < deadline:
        realized = H.x_root_size()
        if abs(realized[0] - w) <= 16 and abs(realized[1] - h) <= 16:
            break
        await asyncio.sleep(0.3)
    return realized


async def recv_clipboard(ws: Any, timeout: float) -> tuple:
    """Collect the tagged read-back reply.

    Returns:
        `(tagged, payload)` -- whether `clipboard_reply,cr` arrived and the
        bytes of the `clipboard` frame after it, both None-safe.
    """
    tagged, payload = False, None
    deadline = time.time() + timeout
    while payload is None:
        remaining = deadline - time.time()
        if remaining <= 0:
            break
        try:
            msg = await asyncio.wait_for(ws.recv(), timeout=remaining)
        except asyncio.TimeoutError:
            break
        if not isinstance(msg, str):
            continue
        if msg == "clipboard_reply,cr":
            tagged = True
        elif msg.startswith("clipboard,"):
            payload = base64.b64decode(msg.split(",", 1)[1])
    return tagged, payload


def run() -> "H.Results":
    """Drive a read-back nobody answers, then one that is answered."""
    res = H.Results("clipboard-read-stall")
    uri = f"ws://localhost:{H.PORT}/api/websockets"

    H.server_start(mode="websockets", wayland=False)
    owner = silent_owner()

    async def behind_a_stalled_read():
        async with websockets.connect(uri, max_size=None) as ws:
            await asyncio.wait_for(ws.recv(), timeout=10)
            await ws.send(settings_payload())
            sized = await wait_root(INITIAL_W, INITIAL_H, 30)
            res.check("the session comes up at the size its settings asked for",
                      abs(sized[0] - INITIAL_W) <= 16, f"root={sized}")
            await ws.send("cr")
            await ws.send(f"r,{WANT_W}x{WANT_H},primary")
            realized = await wait_root(WANT_W, WANT_H, 20)
            res.check("a resize behind an unanswered read-back is still applied",
                      abs(realized[0] - WANT_W) <= 16 and abs(realized[1] - WANT_H) <= 16,
                      f"root={realized}")

    try:
        asyncio.run(behind_a_stalled_read())
    finally:
        owner.close()

    # A fresh server, so the read-backs the silent owner is still holding do
    # not queue this one behind them inside the selection monitor.
    H.server_start(mode="websockets", wayland=False)
    served, stop = H.x_own_clipboard(PAYLOAD)

    async def answered_read():
        async with websockets.connect(uri, max_size=None) as ws:
            await asyncio.wait_for(ws.recv(), timeout=10)
            await ws.send(settings_payload())
            await ws.send("cr")
            tagged, payload = await recv_clipboard(ws, 20)
            res.check("an answered read-back comes back tagged", tagged, "")
            res.check("an answered read-back carries the selection",
                      payload == PAYLOAD, repr(payload)[:64])

    try:
        asyncio.run(answered_read())
    finally:
        stop["flag"] = True
        served.close()
        H.server_stop()

    res.summary()
    return res


if __name__ == "__main__":
    r = run()
    sys.exit(0 if not r.failed() else 1)
