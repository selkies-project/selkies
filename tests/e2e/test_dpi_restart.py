#!/usr/bin/env python3
"""The desktop's density across a server restart.

On X11 a page's density is written into the home as Xft resources, which the
image loads again at the next start, so the desktop comes up at the last page's
density while the server starts knowing none. The server reads the desktop
before writing to it: a page at unity brings a desktop left at 2x back down,
a page at that density rewrites nothing, an operator-set density is applied at
startup even at unity, and a fresh home at unity stays untouched. On Wayland a
density is a compositor scale that nothing persists: a page streams at its own
density after a restart, and resources an X11 session left in the home are
neither applied nor rewritten. Both transports.

Uses `E2E_DISPLAY` when set; otherwise starts a throwaway Xvfb.
Usage: python3 tests/e2e/test_dpi_restart.py [websockets|webrtc|all] [x11|wayland|all]
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

MODES = ("websockets", "webrtc")
FILES = (".Xresources", ".xsettingsd")


def x_env() -> dict:
    return {"DISPLAY": H.TEST_DISPLAY, "PATH": os.environ.get("PATH", "")}


def xft_dpi() -> Optional[int]:
    """Xft.dpi in the display's resource database, None when unset."""
    out = subprocess.run(["xrdb", "-query"], capture_output=True, text=True, env=x_env()).stdout
    for line in out.splitlines():
        if line.startswith("Xft.dpi:"):
            return int(line.split(":", 1)[1])
    return None


def persist(home: str, dpi: Optional[int]) -> None:
    """The resources a session left in the home; None is a home that never had
    a density."""
    for name in FILES:
        try:
            os.remove(os.path.join(home, name))
        except FileNotFoundError:
            pass
    if dpi is None:
        return
    with open(os.path.join(home, ".Xresources"), "w") as f:
        f.write(f"Xft.dpi:   {dpi}\n")
    with open(os.path.join(home, ".xsettingsd"), "w") as f:
        f.write(f"Xft/DPI {dpi * 1024}\n")


def leave_desktop_at(home: str, dpi: Optional[int]) -> None:
    """The state a restart finds on X11: the persisted resources, merged into
    the server as an image does at session start."""
    subprocess.run(["xrdb", "-remove"], capture_output=True, env=x_env())
    persist(home, dpi)
    if dpi is not None:
        subprocess.run(["xrdb", "-merge", os.path.join(home, ".Xresources")], check=True, env=x_env())


def persisted(home: str) -> dict:
    """Each persisted file's density and mtime."""
    found = {}
    for name in FILES:
        path = os.path.join(home, name)
        try:
            with open(path) as f:
                digits = [int(t) for t in f.read().replace(":", " ").split() if t.isdigit()]
        except FileNotFoundError:
            continue
        found[name] = (digits[-1] // (1024 if name == ".xsettingsd" else 1),
                       os.stat(path).st_mtime_ns)
    return found


def densities(home: str) -> set:
    return {dpi for dpi, _ in persisted(home).values()}


def wait_for(pred, timeout: float = 20) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pred():
            return True
        time.sleep(0.25)
    return False


BUFFER_WIDTH_JS = """() => {
  const c = document.getElementById('videoCanvas'), v = document.getElementById('stream');
  return (c && c.width > 0) ? c.width : (v && v.videoWidth > 0) ? v.videoWidth : 0;
}"""


def stream_scale(ctx: Any) -> float:
    """Stream pixels per CSS pixel of the page's 1280-wide viewport."""
    return ctx.pages[0].evaluate(BUFFER_WIDTH_JS) / 1280


def page_at(browser: Any, mode: str, dpr: int) -> Any:
    """A streaming page on a screen of the given density; returns its context."""
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=dpr)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    video = (C.wait_wr_video(page, timeout=45) if mode == "webrtc"
             else C.wait_ws_video(page, timeout=45))
    if not video:
        ctx.close()
        raise RuntimeError(f"no {mode} video for a {dpr}x page; see {H.LOG}")
    return ctx


def run_mode(res: H.Results, browser: Any, mode: str, home: str) -> None:
    applied = "DPI changed from 192 to 96" if mode == "websockets" else "Successfully set DPI to 96"

    leave_desktop_at(home, 192)
    H.server_start(mode, extra_env={"HOME": home})
    ctx = page_at(browser, mode, 1)
    res.check(f"{mode}: a 1x page brings a desktop left at 192 back to 96",
              wait_for(lambda: xft_dpi() == 96), xft_dpi())
    res.check(f"{mode}: the persisted resources follow",
              wait_for(lambda: densities(home) == {96}), persisted(home))
    with open(H.LOG) as f:
        res.check(f"{mode}: the page's density read as a change from 192",
                  applied in f.read(), applied)
    ctx.close()

    leave_desktop_at(home, 192)
    H.server_start(mode, extra_env={"HOME": home})
    time.sleep(3)
    before = persisted(home)
    ctx = page_at(browser, mode, 2)
    time.sleep(3)
    res.check(f"{mode}: a 2x page leaves a desktop at 192 where it is",
              xft_dpi() == 192, xft_dpi())
    res.check(f"{mode}: and rewrites nothing", persisted(home) == before,
              (before, persisted(home)))
    ctx.close()

    leave_desktop_at(home, 192)
    H.server_start(mode, extra_env={"HOME": home, "SELKIES_SCALING_DPI": "96"})
    res.check(f"{mode}: an operator-set 96 resets a desktop left at 192 before any page",
              wait_for(lambda: xft_dpi() == 96 and densities(home) == {96}),
              (xft_dpi(), persisted(home)))
    ctx = page_at(browser, mode, 2)
    time.sleep(3)
    res.check(f"{mode}: and a 2x page cannot move it",
              xft_dpi() == 96 and densities(home) == {96}, (xft_dpi(), persisted(home)))
    ctx.close()

    leave_desktop_at(home, None)
    H.server_start(mode, extra_env={"HOME": home})
    ctx = page_at(browser, mode, 1)
    time.sleep(3)
    res.check(f"{mode}: a fresh home at unity is left untouched",
              xft_dpi() is None and persisted(home) == {}, (xft_dpi(), persisted(home)))
    ctx.close()


def run_wayland(res: H.Results, browser: Any, mode: str, home: str) -> None:
    persist(home, 192)
    H.server_start(mode, wayland=True, extra_env={"HOME": home})
    before = persisted(home)
    ctx = page_at(browser, mode, 2)
    res.check(f"{mode} on Wayland: a 2x page streams at its own density",
              wait_for(lambda: abs(stream_scale(ctx) - 2) < 0.05), stream_scale(ctx))
    ctx.close()

    H.server_start(mode, wayland=True, extra_env={"HOME": home})
    ctx = page_at(browser, mode, 1)
    res.check(f"{mode} on Wayland: after a restart a 1x page streams at unity",
              wait_for(lambda: abs(stream_scale(ctx) - 1) < 0.05), stream_scale(ctx))
    res.check(f"{mode} on Wayland: X resources left in the home are neither applied nor rewritten",
              persisted(home) == before, (before, persisted(home)))
    ctx.close()


def main() -> int:
    want = sys.argv[1] if len(sys.argv) > 1 else "all"
    modes = MODES if want == "all" else (want,)
    backends = sys.argv[2] if len(sys.argv) > 2 else "all"
    res = H.Results("dpi-restart")
    xproc = None
    if backends != "wayland":
        xproc, H.TEST_DISPLAY = H.private_x_server(width=1280, height=720)
    home = tempfile.mkdtemp(prefix="selkies-dpi-home-")
    try:
        with sync_playwright() as pw:
            browser = C.launch_browser(pw, "chromium")
            try:
                for mode in modes:
                    if backends != "wayland":
                        run_mode(res, browser, mode, home)
                    if backends != "x11":
                        run_wayland(res, browser, mode, home)
            finally:
                browser.close()
    finally:
        H.server_stop()
        shutil.rmtree(home, ignore_errors=True)
        if xproc:
            H.stop_x_server(xproc, H.TEST_DISPLAY)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
