#!/usr/bin/env python3
"""A second screen comes up, and comes up in its own place, on every transport.

test_matrix already drives a #display2 page far enough to see the config update
and a video element. What it cannot see is where the server put that display:
the secondary's offset is computed from the primary's size, and when the two
disagree the compositor rejects the output as overlapping and the second screen
never appears at all. Each combination here asserts the applied geometry.

Pixel content is proven separately, without a browser, by
integration/test_two_display_pixels.py.
"""
import ast
import os
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

from typing import Optional


def desktop_window() -> Optional[tuple]:
    """(x, y, w, h) of the session's desktop window, or None when it has none.

    A session drawing natively on Wayland has no X11 desktop window at all, which
    is not the same as one that is misplaced.
    """
    env = {**os.environ, "DISPLAY": H.require_display()}
    listing = subprocess.run(["wmctrl", "-l", "-G", "-x"], capture_output=True,
                             text=True, env=env).stdout
    for line in listing.splitlines():
        fields = line.split()
        if len(fields) >= 7 and fields[6].startswith("pcmanfm-qt"):
            return tuple(int(v) for v in fields[2:6])
    return None


def wait_secondary_ready(mode: str, secondary_id: str = "display2", timeout: float = 45) -> bool:
    """Block until the server has started the secondary capture.

    The transports announce it differently: websockets logs the per-display
    capture start, WebRTC logs the secondary pipeline with its applied rect.
    """
    if mode == "webrtc":
        return C.wait_log("Secondary display '{}' pipeline started".format(secondary_id),
                          timeout=timeout)
    return C.wait_log("SUCCESS: Capture started for '{}'".format(secondary_id), timeout=timeout)


def primary_size(page, mode: str, timeout: float = 20) -> Optional[dict]:
    """The primary's video size once it matches what the client asked for.

    The first frames can still carry the size the display had before the
    client's resolution request landed -- a root left large by an earlier
    session, say -- and WebRTC renegotiates to the requested size a moment
    later. Sampling once at first frame therefore reads a size the layout no
    longer has.
    """
    want = (page.viewport_size or {}).get("width")
    deadline = time.time() + timeout
    info = None
    while time.time() < deadline:
        info = (C.wait_wr_video(page, timeout=5) if mode == "webrtc"
                else C.wait_ws_video(page, timeout=5))
        if not want or (info and info["w"] == want):
            return info
        time.sleep(0.5)
    return info


def server_layout(mode: str, primary_wh: tuple) -> Optional[dict]:
    """The layout the server applied, as `{display_id: rect}`."""
    if mode == "webrtc":
        out = subprocess.run(["grep", "-a", "pipeline started at", H.LOG],
                             capture_output=True, text=True).stdout.strip().splitlines()
        if not out:
            return None
        rect = ast.literal_eval(out[-1].split("pipeline started at ", 1)[1])
        return {"primary": {"x": 0, "y": 0, "w": primary_wh[0], "h": primary_wh[1]},
                "display2": rect}
    out = subprocess.run(["grep", "-a", "Layout calculated", H.LOG],
                         capture_output=True, text=True).stdout.strip().splitlines()
    if not out:
        return None
    tail = out[-1].split("Layouts: ", 1)
    return ast.literal_eval(tail[1]) if len(tail) > 1 else None


def wait_root_width(width: int, timeout: float = 30) -> Optional[tuple]:
    """Block until the X root has grown to at least the union width."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        got = H.x_root_size()
        if got and got[0] >= width:
            return got
        time.sleep(0.5)
    return None


def start_window_manager(wayland: bool) -> Optional[subprocess.Popen]:
    """Openbox on the X11 test display, the manager measured to read the
    monitor set only as it starts; None on Wayland or without one on PATH."""
    if wayland or shutil.which("openbox") is None:
        return None
    display = H.require_display()
    return H.spawn(["openbox", "--replace"], env={**os.environ, "DISPLAY": display},
                   stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)


def wm_restart_check(res: "H.Results", wm_proc: Optional[subprocess.Popen], mark: int) -> None:
    """The first extend restarts Openbox so it reads the new monitor set: the
    server says so, the process it started with exits, and a fresh one manages."""
    if wm_proc is None:
        res.skip("the first extend restarts the window manager", "no Openbox on the display")
        return
    from selkies import display_utils as DU
    os.environ["DISPLAY"] = H.require_display()
    DU._drop_module_display()
    deadline = time.time() + 20
    said = False
    while time.time() < deadline and not (said and wm_proc.poll() is not None):
        said = said or H.server_log().find("restarting openbox", mark) >= 0
        time.sleep(0.25)
    new_pid = DU._sync_wm_pid()
    res.check("the first extend restarts the window manager",
              said and wm_proc.poll() is not None and new_pid not in (0, wm_proc.pid)
              and DU.wm_name_matches("openbox", DU._sync_wm_name()),
              f"said={said} old={wm_proc.pid} exit={wm_proc.poll()} new={new_pid}")
    if new_pid not in (0, wm_proc.pid):
        os.kill(new_pid, 15)


def run(mode: str, wayland: bool) -> bool:
    """Assert the applied two-display geometry for one transport/backend pair."""
    tag = "two-display-{}-{}".format(mode, "wl" if wayland else "x11")
    res = H.Results(tag)
    wm_proc = start_window_manager(wayland)
    H.server_start(mode=mode, wayland=wayland)

    with sync_playwright() as p:
        browser, page, console_errors, not_found = C.launch_chrome(p, mode=mode)
        try:
            info = (C.wait_wr_video(page, timeout=45) if mode == "webrtc"
                    else C.wait_ws_video(page, timeout=20))
            res.check("primary video flows", info is not None, info)
            if not info:
                return res.summary()
            info = primary_size(page, mode) or info

            log_mark = len(H.server_log())
            dpage = C.new_page(browser.contexts[0], mode=mode, url_hash="#display2-right")
            res.check("secondary capture started server-side", wait_secondary_ready(mode), "")

            layout = server_layout(mode, (info["w"], info["h"]))
            res.check("both displays in the layout",
                      bool(layout) and "primary" in layout and "display2" in layout, layout)
            if not layout or "display2" not in layout:
                return res.summary()

            prim, sec = layout["primary"], layout["display2"]
            res.check("secondary is offset, not mirrored",
                      (sec["x"], sec["y"]) != (prim["x"], prim["y"]),
                      "primary @{},{} secondary @{},{}".format(
                          prim["x"], prim["y"], sec["x"], sec["y"]))
            res.check("secondary sits beside the primary, no overlap",
                      sec["x"] >= prim["x"] + prim["w"] or sec["y"] >= prim["y"] + prim["h"],
                      "primary {}x{}@{},{} secondary {}x{}@{},{}".format(
                          prim["w"], prim["h"], prim["x"], prim["y"],
                          sec["w"], sec["h"], sec["x"], sec["y"]))

            if not wayland:
                union_w = max(r["x"] + r["w"] for r in layout.values())
                res.check("framebuffer grew to the union",
                          wait_root_width(union_w) is not None, union_w)
                wm_restart_check(res, wm_proc, log_mark)
            else:
                res.check("secondary output has its own geometry",
                          sec["w"] > 0 and sec["h"] > 0, sec)

            # A root that matches the monitors still shows a broken desktop when the
            # desktop window itself is placed off the layout origin: the far screen
            # is then left with no wallpaper, icons or menu, and the root's own far
            # edge is uncovered.
            if shutil.which("wmctrl") is None:
                res.skip("desktop window covers the layout",
                         "wmctrl not installed; cannot list X11 windows")
            elif (desktop := desktop_window()) is None:
                res.skip("desktop window covers the layout",
                         "no X11 desktop window (a native Wayland session has none)")
            else:
                x, y, w, h = desktop
                root_w, root_h = H.x_root_size()
                res.check("desktop window covers the layout",
                          x == 0 and y == 0 and w >= root_w and h >= root_h,
                          "desktop {}x{}@{},{} root {}x{}".format(
                              w, h, x, y, root_w, root_h))

            dpage.close()
            time.sleep(2.0)
            after = (C.wait_wr_video(page, timeout=10) if mode == "webrtc"
                     else C.wait_ws_video(page, timeout=10))
            res.check("primary survives secondary teardown", after is not None, after)

            real_errors, _ = C.benign_console(console_errors, not_found)
            res.check("no console errors", len(real_errors) == 0, "; ".join(real_errors)[:200])
        finally:
            browser.close()
    return res.summary()


SELECTORS = {
    "websockets-x11": ("websockets", False),
    "webrtc-x11": ("webrtc", False),
    "websockets-wl": ("websockets", True),
    "webrtc-wl": ("webrtc", True),
}

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    chosen = SELECTORS.values() if which == "all" else [SELECTORS[which]]
    ok = True
    try:
        for mode, wl in chosen:
            ok = run(mode, wl) and ok
    finally:
        H.server_stop()
    sys.exit(0 if ok else 1)
