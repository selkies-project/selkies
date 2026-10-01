#!/usr/bin/env python3
"""Scroll (wheel) input parity e2e: Chromium over websockets and webrtc, both
X11 and Wayland. Verifies REL_WHEEL buttons reach the X server (X11) or the
compositor seat sees scroll (Wayland, via WlObs). On Wayland a touchpad's
stroke reaches the seat as a finger's: fractional values from a finger source,
as far as the page scrolled, then an axis stop once the stroke pauses."""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright


def wheel_block(mode: str, wayland: bool, block: str) -> "H.Results":
    """Scroll in a real browser and verify the events land server-side.

    Args:
        mode: Transport mode, ``websockets`` or ``webrtc``.
        wayland: True to run against the Wayland backend, False for X11.
        block: Label for this block's Results; derived from mode when empty.

    Returns:
        The Results accumulator for this block's checks.
    """
    tag = block or f"{mode}-{'wl' if wayland else 'x11'}"
    res = H.Results(tag)
    wl = "wayland-1"
    H.server_start(mode=mode, wayland=wayland)

    with sync_playwright() as p:
        browser, page, console_errors, not_found = C.launch_chrome(p, mode=mode)
        wl_obs = None
        try:
            if mode == "websockets":
                info = C.wait_ws_video(page)
            else:
                info = C.wait_wr_video(page, timeout=45)
            res.check("video up", info is not None, info)
            time.sleep(1.0)
            page.mouse.click(640, 360)
            time.sleep(0.5)
            if wayland:
                wl_obs = H.WlObs(wl)
                wl_obs.ready()
                page.mouse.move(10, 10)
            for _ in range(4):
                page.mouse.wheel(0, 120)
                time.sleep(0.4)

            # X11: wheel = button 4 (up) / 5 (down). Watch our own override-redirect
            # window (mapped without a WM) and verify XTEST ButtonPress 4/5.
            if not wayland:
                d, events, stop = H.x_key_watcher()
                w = d.screen().width_in_pixels // 2
                h = d.screen().height_in_pixels // 2
                time.sleep(0.3)
                page.mouse.move(w, h)
                time.sleep(0.5)
                page.mouse.wheel(0, 120)
                time.sleep(0.3)
                page.mouse.wheel(0, -120)
                time.sleep(0.8)
                stop["flag"] = True
                d.close()
                detail5 = sum(1 for n, det in events if n == "ButtonPress" and det == 5)
                sdetail4 = sum(1 for n, det in events if n == "ButtonPress" and det == 4)
                res.check("X11: wheel down reached server (Button5)",
                          detail5 >= 1, events[:6])
                res.check("X11: wheel up reached server (Button4)",
                          sdetail4 >= 1, events[:6])
            else:
                time.sleep(1.0)
                ev = wl_obs.wait_for("ptr_axis", timeout=5)
                res.check("Wayland: pointer axis reached compositor", ev is not None, ev)
                # Opposite direction: the compositor must see a second axis
                # event rather than a repeat of the first.
                page.mouse.wheel(0, -120)
                time.sleep(0.8)
                res.check("Wayland: second pointer axis reached compositor",
                          wl_obs.wait_for("ptr_axis", timeout=5) is not None)
                # The page has a wheel's samples only at its fourth notch;
                # the ones before are a wheel's all the same.
                wheel_sources = [l["source"] for l in wl_obs.lines if l.get("kind") == "ptr_axis_source"]
                res.check("Wayland: a wheel's notches come from a wheel source, its first ones too",
                          bool(wheel_sources) and set(wheel_sources) == {0}, wheel_sources[:8])
                # Past the page's one-second wheel reset, a stroke of small pixel
                # deltas is a touchpad's.
                time.sleep(1.3)
                mark = len(wl_obs.lines)
                stroke = [3.5, 6.25, 9.0, 4.75, 2.5]
                for d in stroke:
                    page.mouse.wheel(0, d)
                    time.sleep(0.016)
                time.sleep(0.5)
                seen = [l for l in wl_obs.lines[mark:]
                        if l.get("kind") in ("ptr_axis", "ptr_axis_source", "ptr_axis_stop")]
                values = [l["value"] for l in seen if l["kind"] == "ptr_axis" and l.get("axis") == 0]
                sources = {l["source"] for l in seen if l["kind"] == "ptr_axis_source"}
                last_axis = max((i for i, l in enumerate(seen) if l["kind"] == "ptr_axis"), default=-1)
                stopped = any(l["kind"] == "ptr_axis_stop" for l in seen[last_axis + 1:])
                res.check("Wayland: a touchpad's stroke comes from a finger source", sources == {1}, seen[:4])
                res.check("Wayland: in fractional values, as far as the page scrolled",
                          any(v != int(v) for v in values) and abs(sum(values) - sum(stroke)) < 0.1,
                          f"{values} against {sum(stroke)}")
                res.check("Wayland: and stops once the stroke pauses", stopped, seen[-3:])
            real, bad = C.benign_console(console_errors, not_found)
            res.check("no console errors", len(real) == 0, "; ".join(real)[:120])
        finally:
            browser.close()
            if wl_obs is not None:
                wl_obs.stop()
    res.summary()
    return res


def main() -> None:
    """Run the wheel block for each transport/backend pair named on argv."""
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    for mode, wl in (("websockets", False), ("webrtc", False), ("websockets", True), ("webrtc", True)):
        key = f"{mode}-{'wl' if wl else 'x11'}"
        if which in ("all", key):
            blocks.append(wheel_block(mode, wl, key))
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== SCROLL: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()