#!/usr/bin/env python3
"""Two displays cut out of one Wayland screen, and the seam between them.

Each display is a view of one screen rather than a screen of its own, so the
session sees a single desk: a window that overlaps the boundary is composited
onto both views, each drawing the part that falls in its own rectangle, and --
the point of the whole arrangement -- a pointer grab crosses the boundary
instead of stopping at it. A drag runs under such a grab, so with a screen per
display a window cannot be dragged from one to the next: the grab clamps at the
first screen's edge, which is what this pins against.

The pixels come from what each display's capture actually delivers, and the
grab from a client of the NESTED compositor, since that is the chain a desktop
session runs through.

Usage: python3 tests/integration/test_wayland_seam.py
"""
import io
import os
import subprocess
import sys
import threading
import time
from typing import Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

RUNTIME = os.path.join(H.WORKDIR, "wl-seam")
#: One display's size; the screen the two are cut from is twice as wide.
DISPLAY = (1920, 1080)
SPAN = (DISPLAY[0] * 2, DISPLAY[1])
# The colour the walked window is painted, and how much of a display it has to
# cover to count as present there (a 500x400 window is ~10% of one screen).
WIN_RGB = (240, 20, 20)
PRESENT = 0.005


class Sink:
    """Keeps the newest JPEG payload one display delivered."""

    def __init__(self) -> None:
        self.last: Optional[bytes] = None
        self.n = 0
        self.lock = threading.Lock()

    def __call__(self, frame) -> None:
        payload = bytes(frame)
        start = payload.find(b"\xff\xd8")
        if start < 0:
            return
        with self.lock:
            self.n += 1
            self.last = payload[start:]

    def coverage(self, rgb: Tuple[int, int, int], tol: int = 70) -> float:
        """Fraction of the newest frame within `tol` of `rgb`."""
        from PIL import Image

        with self.lock:
            data = self.last
        if data is None:
            return 0.0
        px = list(Image.open(io.BytesIO(data)).convert("RGB").resize((160, 90)).getdata())
        near = sum(1 for p in px if all(abs(p[i] - rgb[i]) <= tol for i in range(3)))
        return near / len(px)


def settings(display_id: int):
    """A JPEG capture of one display-sized view."""
    import pixelflux

    cs = pixelflux.CaptureSettings()
    cs.use_wayland = True
    cs.display_id = display_id
    cs.capture_width, cs.capture_height = DISPLAY
    cs.target_fps = 10.0
    cs.output_mode = 0
    cs.jpeg_quality = 80
    cs.use_cpu = True
    cs.capture_cursor = False
    return cs


def nested(socket: str, config: str) -> subprocess.Popen:
    """Start a decorated nested labwc on the one screen, with XWayland."""
    startup = os.path.join(RUNTIME, "startup.sh")
    with open(startup, "w") as fh:
        fh.write(f"#!/bin/bash\nenv | grep ^DISPLAY= > {RUNTIME}/env\nexec sleep 3600\n")
    os.chmod(startup, 0o755)
    # No screen count is set: a nested wlroots opens one by default, which is
    # what the displays are cut from.
    env = dict(os.environ, WAYLAND_DISPLAY=socket, WLR_BACKENDS="wayland",
               XDG_RUNTIME_DIR=RUNTIME, WLR_RENDERER="pixman", XDG_CONFIG_HOME=config,
               LIBGL_ALWAYS_SOFTWARE="1")
    env.pop("DISPLAY", None)
    # Xwayland inherits this: its glamor probe segfaults inside the NVIDIA EGL
    # vendor when the renderer underneath is software.
    mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json"
    if os.path.exists(mesa):
        env["__EGL_VENDOR_LIBRARY_FILENAMES"] = mesa
    log = open(os.path.join(RUNTIME, "labwc.log"), "w")
    return H.spawn(["labwc", "-s", startup], env=env, stdout=log,
                   stderr=subprocess.STDOUT)


def session(socket: str) -> Tuple[str, str]:
    """The nested compositor's own socket and the X display it serves."""
    inner, display = "", ""
    for _ in range(160):
        names = [n for n in sorted(os.listdir(RUNTIME))
                 if n.startswith("wayland-") and not n.endswith(".lock") and n != socket]
        if names:
            inner = names[0]
        dump = os.path.join(RUNTIME, "env")
        if os.path.exists(dump):
            display = open(dump).read().strip().split("=", 1)[-1]
        if inner and display:
            break
        time.sleep(0.25)
    return inner, display


class Patch:
    """A solid-coloured window on the session's X display, placed by this test.

    Override-redirect so its position is the test's to state rather than the
    window manager's, and drawn by the X server from the window background, so
    the suite needs no client program of its own.
    """

    def __init__(self, display: str, rgb: Tuple[int, int, int]) -> None:
        from selkies.Xlib import X, display as xdisplay

        self.d = xdisplay.Display(display)
        root = self.d.screen().root
        self.win = root.create_window(
            300, 300, 500, 400, 0, X.CopyFromParent, X.InputOutput,
            X.CopyFromParent, override_redirect=True,
            background_pixel=(rgb[0] << 16) | (rgb[1] << 8) | rgb[2])
        self.win.map()
        self.d.sync()
        time.sleep(1.5)

    def move_to(self, x: int) -> None:
        """Put the window's left edge at `x` and let the frame settle."""
        self.win.configure(x=x, y=300)
        self.d.sync()
        time.sleep(2.5)

    def close(self) -> None:
        self.win.destroy()
        self.d.sync()
        self.d.close()


def main() -> "H.Results":
    """Walk a window across the seam and watch both displays' pixels."""
    os.makedirs(RUNTIME, mode=0o700, exist_ok=True)
    os.environ["XDG_RUNTIME_DIR"] = RUNTIME
    os.environ.setdefault("LIBGL_ALWAYS_SOFTWARE", "1")
    config = os.path.join(RUNTIME, "config")
    os.makedirs(os.path.join(config, "labwc"), exist_ok=True)
    with open(os.path.join(config, "labwc", "rc.xml"), "w") as fh:
        fh.write('<?xml version="1.0"?>\n<labwc_config>\n'
                 '  <core><decoration>server</decoration></core>\n'
                 '</labwc_config>\n')
    import pixelflux

    res = H.Results("wl-seam")
    socket = pixelflux.ensure_wayland_display(width=SPAN[0], height=SPAN[1],
                                              render_node="", auto_gpu="", cursor_size=-1)
    left_cap, right_cap = pixelflux.ScreenCapture(), pixelflux.ScreenCapture()
    # The screen carries the first display from the start; the second is cut
    # from it beside the first.
    default = [o[0] for o in left_cap.list_outputs()]
    res.check("the screen comes with one display on it", default == [0, 1], default)
    res.check("a second display is cut from the same screen",
              left_cap.create_view(2, 0, DISPLAY[0], 0, *DISPLAY))
    left, right = Sink(), Sink()
    left_cap.start_capture(left, settings(1))
    right_cap.start_capture(right, settings(2))
    time.sleep(2.5)

    proc = nested(socket, config)
    patch = None
    try:
        inner, display = session(socket)
        if not inner or not display:
            H.skip_suite("the nested compositor did not come up")
        time.sleep(1.5)
        patch = Patch(display, WIN_RGB)
        patch.move_to(300)
        first = (left.coverage(WIN_RGB), right.coverage(WIN_RGB))
        res.check("a window on the first display shows only there",
                  first[0] > PRESENT and first[1] <= PRESENT, first)

        patch.move_to(DISPLAY[0] - 250)
        across = (left.coverage(WIN_RGB), right.coverage(WIN_RGB))
        res.check("a window over the boundary shows on both",
                  across[0] > PRESENT and across[1] > PRESENT, across)

        patch.move_to(DISPLAY[0] + 400)
        second = (left.coverage(WIN_RGB), right.coverage(WIN_RGB))
        res.check("a window past the boundary shows only on the second",
                  second[0] <= PRESENT and second[1] > PRESENT, second)

        # What a drag rides on. The grab is held across the boundary: with a
        # screen per display it clamps at the first screen's last column, and
        # the window being dragged stops there with it.
        obs = H.WlObs(inner)
        if not obs.ready(20):
            res.skip("a held grab crosses the boundary", "no observer surface")
        else:
            time.sleep(1.0)
            obs.lines.clear()
            left_cap.inject_mouse_move(500.0, 400.0)
            time.sleep(0.4)
            left_cap.inject_mouse_button(1, 1)
            time.sleep(0.3)
            for x in range(500, SPAN[0] - 40, 50):
                left_cap.inject_mouse_move(float(x), 400.0)
                time.sleep(0.03)
            time.sleep(0.6)
            left_cap.inject_mouse_button(1, 0)
            seen = [ln["x"] for ln in obs.lines if ln.get("kind") == "ptr_motion"]
            clicks = [ln for ln in obs.lines if ln.get("kind") == "ptr_button"]
            reach = max(seen) if seen else -1.0
            res.check("a held grab crosses the boundary", reach > DISPLAY[0] + 500,
                      f"reached x={reach}, boundary at {DISPLAY[0]}")
            res.check("injected buttons reach the session's clients",
                      len(clicks) >= 1, clicks)
            obs.proc.terminate()
    finally:
        if patch is not None:
            patch.close()
        proc.terminate()
        left_cap.stop_capture()
        right_cap.stop_capture()
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
