#!/usr/bin/env python3
"""Second-display placement on the Wayland backend, driven at the compositor
level because that is where it breaks: a nested compositor (labwc here, as in
any wlroots desktop) opens one host toplevel per screen, and those toplevels
must land on separate outputs rather than stacking on the first one.

The placement decision runs on a toplevel's first commit. It cannot key off
xdg's initial_configure_sent, because clients that negotiate xdg-decoration are
answered with a configure before they ever commit.
"""
import os
import subprocess
import sys
import time
from typing import List, Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

RUNTIME = os.path.join(H.WORKDIR, "wl-multi-output")


def nested_socket_name(capture_socket: str) -> Optional[str]:
    """The nested compositor's own socket, which is the one that is not the
    capture socket. Returns None when it has not appeared."""
    for name in sorted(os.listdir(RUNTIME)):
        if (name.startswith("wayland-") and not name.endswith(".lock")
                and name != capture_socket):
            return name
    return None


def screen_positions(socket: str) -> List[Tuple[int, int]]:
    """The nested compositor's screen origins, in screen order, read through
    its own output management."""
    import pixelflux
    return [(x, y) for _name, x, y in
            pixelflux.ScreenCapture().list_app_screens(socket)]


def compositor_env(socket: str) -> dict:
    """Environment for a nested wlroots compositor on our own socket."""
    env = dict(os.environ)
    env.update(WAYLAND_DISPLAY=socket, WLR_BACKENDS="wayland",
               XDG_RUNTIME_DIR=RUNTIME, WLR_RENDERER="pixman",
               LIBGL_ALWAYS_SOFTWARE="1")
    env.pop("DISPLAY", None)
    return env


def windows_after(capture, count: int, timeout: float = 20) -> list:
    """The window list once `count` of them have mapped, or whatever mapped."""
    deadline = time.time() + timeout
    windows = []
    while time.time() < deadline:
        windows = capture.list_windows()
        if len(windows) >= count:
            break
        time.sleep(0.5)
    return windows


def nested(socket: str, outputs: int) -> subprocess.Popen:
    """Start labwc nested on the capture socket with the given screen count."""
    return H.spawn(
        ["labwc"], env=dict(compositor_env(socket), WLR_WL_OUTPUTS=str(outputs)),
        stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)


def main() -> "H.Results":
    """Run the placement checks across output create/destroy transitions."""
    os.makedirs(RUNTIME, mode=0o700, exist_ok=True)
    os.environ["XDG_RUNTIME_DIR"] = RUNTIME
    os.environ.setdefault("LIBGL_ALWAYS_SOFTWARE", "1")
    import pixelflux

    if not hasattr(pixelflux.ScreenCapture, "create_output"):
        H.skip_suite("the installed pixelflux has no multi-output API")

    res = H.Results("wl-multi-output")
    socket = pixelflux.ensure_wayland_display(width=1920, height=1080,
                                              render_node="", auto_gpu="",
                                              cursor_size=-1)
    capture = pixelflux.ScreenCapture()

    created = capture.create_output(1, 1920, 1080, 1920, 0, 1.0)
    outputs = capture.list_outputs()
    res.check("second output created", created and len(outputs) == 2, outputs)

    proc = nested(socket, 2)
    try:
        windows = windows_after(capture, 2)
        placed = sorted({w[3] for w in windows})
        res.check("nested compositor screens land on separate outputs",
                  len(windows) >= 2 and len(placed) >= 2,
                  f"{len(windows)} toplevel(s) on outputs {placed}")

        # A nested compositor arranges the screens it opens by its own rule —
        # wlroots places them side by side, whatever the capture layout says —
        # and that arrangement is the one windows are placed and dragged in, so
        # a display asked for above or to the left of the primary lands right of
        # it until the session is told otherwise. Every arrangement is checked,
        # since an overlap on the way to one is what a screen-at-a-time apply
        # gets wrong.
        nested_socket = nested_socket_name(socket)
        for name, rects in (
            ("left", [(1920, 0, 1920, 1080), (0, 0, 1920, 1080)]),
            ("up", [(0, 1080, 1920, 1080), (0, 0, 1920, 1080)]),
            ("down", [(0, 0, 1920, 1080), (0, 1080, 1920, 1080)]),
            ("right", [(0, 0, 1920, 1080), (1920, 0, 1920, 1080)]),
        ):
            if nested_socket is None:
                res.check(f"the session lays its screens out for '{name}'",
                          False, "the nested compositor opened no socket")
                break
            try:
                placed_n = capture.set_app_screen_layout(nested_socket, rects)
            except Exception as e:
                res.check(f"the session lays its screens out for '{name}'", False, str(e))
                continue
            time.sleep(1.0)
            got = screen_positions(nested_socket)
            want = [(x, y) for x, y, _w, _h in rects]
            res.check(f"the session lays its screens out for '{name}'",
                      placed_n == len(rects) and got == want,
                      f"placed={placed_n} got={got} want={want}")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    # One screen must still open on the primary rather than being spread.
    capture.destroy_output(1)
    proc = nested(socket, 1)
    try:
        windows = windows_after(capture, 1)
        res.check("single screen stays on the primary output",
                  len(windows) == 1 and windows[0][3] == 0, windows)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    # Screens the layout has no display for wait off-display rather than cover the
    # one the primary is showing, so a session can be started with room for a
    # second display before anyone connects to it.
    proc = nested(socket, 2)
    try:
        windows = windows_after(capture, 2)
        waiting = [w for w in windows if w[4]]
        res.check("the spare screen waits instead of covering the primary",
                  len(windows) >= 2 and len(waiting) == 1,
                  f"{len(windows)} toplevel(s), {len(waiting)} waiting")
        res.check("a new output takes the waiting screen",
                  capture.create_output(1, 1920, 1080, 1920, 0, 1.0)
                  and not any(w[4] for w in windows_after(capture, 2)),
                  capture.list_windows())
        capture.destroy_output(1)
        time.sleep(1)
        res.check("the screen waits again when its output goes away",
                  sum(1 for w in capture.list_windows() if w[4]) == 1,
                  capture.list_windows())
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
