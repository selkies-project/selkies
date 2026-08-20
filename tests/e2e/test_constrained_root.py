#!/usr/bin/env python3
"""A second screen asked of an X server that will not grow its root.

The layout engines ask for a framebuffer covering every display and then have
to live with what the server gave back. A server whose root is capped below the
request is the case that decides whether a session degrades or breaks: the
primary has to keep streaming at a size that exists, and a secondary with
nowhere to live has to be refused with a verdict the client treats as final --
a bare pipeline failure looks transient and the client reloads into a loop.

The cap is real, not simulated: Xvfb fixes its maximum screen size at the
initial allocation, so RRAddOutputMode and RRSetScreenSize past it fail the way
a driver out of scanout memory fails. Both engines run against the same capped
server, because this is exactly the path where they used to disagree.

The fitting rule itself is pinned branch by branch in
tests/unit/test_realized_layout.py.
"""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

from typing import Optional

# Below the browser viewport the pages run at, so the primary's own request is
# refused too, and small enough that no arrangement of two displays fits.
CAP_W, CAP_H = 1024, 768


def root_max(display: str) -> Optional[tuple]:
    """The largest screen size this server will accept, per RandR."""
    out = subprocess.run(["xrandr", "-display", display], capture_output=True,
                         text=True).stdout
    for line in out.splitlines():
        if line.startswith("Screen ") and "maximum" in line:
            tail = line.split("maximum", 1)[1].strip().rstrip(",")
            w, h = tail.split(" x ")
            return int(w), int(h)
    return None


def secondary_live(mode: str, did: str = "display2") -> bool:
    """Whether the server currently has a running capture for the secondary."""
    if mode == "webrtc":
        return C.wait_log(f"Secondary display '{did}' pipeline started", timeout=12)
    return C.wait_log(f"SUCCESS: Capture started for '{did}'", timeout=12)


def run(mode: str) -> bool:
    """Drive one transport against the capped server."""
    res = H.Results(f"constrained-root-{mode}")
    cap = root_max(H.require_display())
    res.check("the test server really caps its root", cap == (CAP_W, CAP_H), cap)

    H.server_start(mode=mode, wayland=False)
    with sync_playwright() as p:
        browser, page, console_errors, not_found = C.launch_chrome(p, mode=mode)
        try:
            info = (C.wait_wr_video(page, timeout=45) if mode == "webrtc"
                    else C.wait_ws_video(page, timeout=20))
            res.check("primary streams although its own size was refused",
                      info is not None, info)
            if not info:
                return res.summary()
            root = H.x_root_size()
            res.check("the root never grew past the cap",
                      root[0] <= CAP_W and root[1] <= CAP_H, root)
            res.check("the primary is streaming a region that exists",
                      info["w"] <= root[0] and info["h"] <= root[1],
                      "stream {}x{} root {}".format(info["w"], info["h"], root))

            # A secondary to the right starts where the root ends: there is no
            # region for it, and it has to be refused rather than laid out.
            dpage = C.new_page(browser.contexts[0], mode=mode, url_hash="#display2-right")
            res.check("the unplaceable secondary is reported",
                      C.wait_log("does not fit the realized", timeout=45), "")
            res.check("no capture is started outside the root",
                      not secondary_live(mode), "")
            time.sleep(2.0)
            after = (C.wait_wr_video(page, timeout=15) if mode == "webrtc"
                     else C.wait_ws_video(page, timeout=15))
            res.check("the primary survives the refusal", after is not None, after)
            res.check("the root is still inside the cap",
                      all(v <= c for v, c in zip(H.x_root_size(), (CAP_W, CAP_H))),
                      H.x_root_size())
            dpage.close()
            time.sleep(3.0)

            # To the left the arrangement pushes the PRIMARY off the root, which
            # must never cost the session its own screen.
            dpage = C.new_page(browser.contexts[0], mode=mode, url_hash="#display2-left")
            res.check("a primary pushed off the root is re-anchored",
                      C.wait_log("re-anchored at the origin", timeout=45), "")
            res.check("the arrangement that caused it is refused",
                      not secondary_live(mode), "")
            time.sleep(2.0)
            after = (C.wait_wr_video(page, timeout=15) if mode == "webrtc"
                     else C.wait_ws_video(page, timeout=15))
            res.check("the primary still streams after re-anchoring",
                      after is not None, after)
            dpage.close()

            real_errors, _ = C.benign_console(console_errors, not_found)
            res.check("no console errors", len(real_errors) == 0,
                      "; ".join(real_errors)[:200])
        finally:
            browser.close()
            H.server_stop()
    return res.summary()


SELECTORS = ("websockets", "webrtc")

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    chosen = SELECTORS if which == "all" else (which,)
    # Xvfb fixes its maximum screen size at the initial allocation, so a small
    # private server IS a server that refuses to grow. The suite drives its own
    # rather than the harness display, which is deliberately large.
    xvfb, H.TEST_DISPLAY = H.private_x_server(
        CAP_W, CAP_H, extra_args=("+extension", "COMPOSITE", "+extension", "DAMAGE",
                                  "+extension", "RANDR", "+extension", "XFIXES",
                                  "+extension", "XTEST", "-shmem", "-s", "0", "-dpms"))
    ok = True
    try:
        for mode in chosen:
            ok = run(mode) and ok
    finally:
        H.server_stop()
        H.stop_x_server(xvfb, H.TEST_DISPLAY)
    sys.exit(0 if ok else 1)
