#!/usr/bin/env python3
"""A capture that withdraws its cursor callback hands the cursor back.

pixelflux's cursor slot is process-wide, and each display's capture registers
its own callback in it: the WebRTC transport runs one pipeline per display. A
second display leaving withdraws its callback while the primary keeps
capturing, and the primary's callback has to take the slot back, with the
current cursor delivered to it again, or the primary's pages keep whatever
cursor they last received.

Drives two ScreenCapture objects on each backend, the X test display and
pixelflux's own Wayland compositor: the first captures and registers, the
second registers and withdraws. On X11 a changed cursor on screen then has to
reach the primary as well.
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H


def wait_for(events: list, count: int, timeout: float = 5.0) -> bool:
    deadline = time.time() + timeout
    while len(events) < count and time.time() < deadline:
        time.sleep(0.05)
    return len(events) >= count


def cover_screen() -> tuple:
    """Map a raised input-only window over the whole screen, whose cursor is then the
    one on screen wherever the pointer is: a suite before this one can leave a window
    with a cursor of its own under the pointer, where a changed root cursor shows nothing."""
    from selkies.Xlib import X, display as xdisp
    d = xdisp.Display(H.require_display())
    screen = d.screen()
    win = screen.root.create_window(0, 0, screen.width_in_pixels, screen.height_in_pixels, 0, 0,
                                    X.InputOnly, X.CopyFromParent, override_redirect=True)
    win.map()
    win.configure(stack_mode=X.Above)
    d.sync()
    return d, win


def set_cursor(cover: tuple, shape: int) -> None:
    d, win = cover
    font = d.open_font("cursor")
    cursor = font.create_glyph_cursor(font, shape, shape + 1, (0, 0, 0), (65535, 65535, 65535))
    win.change_attributes(cursor=cursor)
    d.sync()


def run(backend: str) -> "H.Results":
    import pixelflux

    wayland = backend == "wayland"
    res = H.Results(f"cursor-callback-handoff-{backend}")
    primary_events: list = []
    secondary_events: list = []
    cs = pixelflux.CaptureSettings()
    cs.capture_width, cs.capture_height = 640, 400
    cs.target_fps = 30.0
    cs.use_cpu = True
    cs.use_wayland = wayland
    cs.codec = "jpeg"
    cs.capture_cursor = False

    cover = None if wayland else cover_screen()
    primary = pixelflux.ScreenCapture()
    secondary = pixelflux.ScreenCapture()
    primary.set_cursor_callback(lambda mt, data, hx, hy: primary_events.append(mt))
    primary.start_capture(lambda frame: None, cs)
    try:
        if cover:
            set_cursor(cover, 150)
        res.check("the primary's callback receives the cursor", wait_for(primary_events, 1), primary_events)

        secondary.set_cursor_callback(lambda mt, data, hx, hy: secondary_events.append(mt))
        res.check("a second capture's registration takes the slot", wait_for(secondary_events, 1),
                  secondary_events)
        before = len(primary_events)
        secondary.clear_cursor_callback()
        res.check("its withdrawal delivers the current cursor to the primary again",
                  wait_for(primary_events, before + 1), f"{len(primary_events) - before} after")

        after_withdrawal = len(secondary_events)
        if wayland:
            time.sleep(1.0)
        else:
            before = len(primary_events)
            set_cursor(cover, 68)
            res.check("a later cursor change reaches the primary", wait_for(primary_events, before + 1),
                      f"{len(primary_events) - before} after")
        res.check("the withdrawn callback hears nothing more", len(secondary_events) == after_withdrawal,
                  f"{len(secondary_events) - after_withdrawal} after")
    finally:
        primary.stop_capture()
        primary.clear_cursor_callback()
        if cover:
            cover[0].close()
    return res


SELECTORS = ("x11", "wayland")

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("x11", "all"):
        os.environ["DISPLAY"] = H.require_display()
    ok = True
    for backend in (SELECTORS if which == "all" else (which,)):
        ok = run(backend).summary() and ok
    sys.exit(0 if ok else 1)
