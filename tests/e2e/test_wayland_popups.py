#!/usr/bin/env python3
"""Popups of clients on pixelflux's own compositor, end to end.

A client (tests/tools/wl_popup_client.py) opens a popup below a 300x40 parent at
the top left: a layer surface's popup, as a panel opens its menu, or a window's.
The popup has to show in the stream, below its parent, and the pointer moved and
clicked over it has to reach the popup's own surface, not its parent's.

Usage: python3 tests/e2e/test_wayland_popups.py [layer|window|all]
"""
import io
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
from PIL import Image  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

PARENT, POPUP = (0x20, 0x60, 0xC0), (0xE0, 0x80, 0x20)


def near(pixel: tuple, want: tuple, tol: int = 40) -> bool:
    return all(abs(a - b) <= tol for a, b in zip(pixel[:3], want))


def popup_block(mode: str) -> "H.Results":
    """Open the client's popup in `mode` ("layer" or "window") and check what
    the stream shows and where the pointer lands."""
    res = H.Results(f"popup-{mode}")
    H.server_start(mode="websockets", wayland=True)
    client = None
    try:
        with sync_playwright() as p:
            browser, page, _, _ = C.launch_chrome(p, mode="websockets")
            try:
                res.check("video up", C.wait_ws_video(page) is not None)
                client = H.WlObs("wayland-1", tool="wl_popup_client.py", POPUP_MODE=mode)
                deadline = time.time() + 10
                while time.time() < deadline and len(
                        {e.get("surface") for e in client.lines if e.get("kind") == "mapped"}) < 2:
                    time.sleep(0.2)
                mapped = {e.get("surface") for e in client.lines if e.get("kind") == "mapped"}
                res.check("the parent and its popup map", mapped >= {"parent", "popup"}, client.lines[:6])
                time.sleep(2.0)
                shot = Image.open(io.BytesIO(page.screenshot())).convert("RGB")
                bar, popup = shot.getpixel((250, 20)), shot.getpixel((100, 110))
                res.check("the parent shows", near(bar, PARENT), bar)
                res.check("the popup shows below it", near(popup, POPUP), popup)
                page.mouse.move(100, 100)
                time.sleep(0.6)
                page.mouse.move(110, 105)
                time.sleep(0.6)
                moves = [e for e in client.lines if e.get("kind") in ("ptr_enter", "ptr_motion")]
                res.check("motion over the popup reaches the popup",
                          any(e.get("surface") == "popup" for e in moves), moves[-3:])
                page.mouse.click(110, 105)
                time.sleep(0.6)
                buttons = [e for e in client.lines if e.get("kind") == "ptr_button"]
                res.check("a click on the popup reaches the popup",
                          any(e.get("surface") == "popup" for e in buttons), buttons[-2:])
            finally:
                browser.close()
    finally:
        if client is not None:
            client.proc.terminate()
        H.server_stop()
    res.summary()
    return res


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = [popup_block(mode) for mode in ("layer", "window") if which in ("all", mode)]
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== POPUPS: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
