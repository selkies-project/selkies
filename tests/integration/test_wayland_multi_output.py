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

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

RUNTIME = os.path.join(H.WORKDIR, "wl-multi-output")


def compositor_env(socket):
    """Environment for a nested wlroots compositor on our own socket."""
    env = dict(os.environ)
    env.update(WAYLAND_DISPLAY=socket, WLR_BACKENDS="wayland",
               XDG_RUNTIME_DIR=RUNTIME, WLR_RENDERER="pixman",
               LIBGL_ALWAYS_SOFTWARE="1")
    env.pop("DISPLAY", None)
    return env


def windows_after(capture, count, timeout=20):
    """The window list once `count` of them have mapped, or whatever mapped."""
    deadline = time.time() + timeout
    windows = []
    while time.time() < deadline:
        windows = capture.list_windows()
        if len(windows) >= count:
            break
        time.sleep(0.5)
    return windows


def nested(socket, outputs):
    return subprocess.Popen(
        ["labwc"], env=dict(compositor_env(socket), WLR_WL_OUTPUTS=str(outputs)),
        stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)


def main():
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
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    # One screen must still open on the primary rather than being spread
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

    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
