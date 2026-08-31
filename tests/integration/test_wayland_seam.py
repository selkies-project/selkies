#!/usr/bin/env python3
"""The seam between two Wayland displays is continuous, so a window crosses it.

Each display is a screen of the capture compositor's own, and the nested
session opens one screen per capture output: two capture outputs side by side
are still one compositor space, not two desktops, so an element that overlaps
both is composited onto both, each output drawing the part that falls in its
own rectangle. That is checked from the pixels each display's capture actually
delivers -- a window walked across the boundary has to appear on the first,
then on both, then on the second.

The pointer that would drive such a drag is checked too: a button held on the
first screen keeps the host delivering motion to that screen's window past its
edge, and the nested compositor has to carry its cursor -- and the window a
drag holds -- onto the second screen rather than clamp it at the first one's
last column. That is the labwc seam patch the container images carry
(`labwc-seam.patch`); a labwc without the patches clamps by design, so the
crossing is skipped there, keyed off the control socket the same patch set
adds. The grab is observed from a client of the NESTED compositor, since that
is the chain a desktop session runs through.

Usage: python3 tests/integration/test_wayland_seam.py
"""
import io
import os
import shutil
import subprocess
import sys
import threading
import time
from typing import Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

RUNTIME = os.path.join(H.WORKDIR, "wl-seam")
#: One display's size; the two screens sit side by side, twice as wide in all.
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
        raw = Image.open(io.BytesIO(data)).convert("RGB").resize((160, 90)).tobytes()
        px = [raw[i : i + 3] for i in range(0, len(raw), 3)]
        near = sum(1 for p in px if all(abs(p[i] - rgb[i]) <= tol for i in range(3)))
        return near / len(px)


def settings(display_id: int):
    """A JPEG capture of one display: the primary's view of screen 0, or a
    secondary's own screen."""
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
    """Start a decorated nested labwc spanning both screens, with XWayland."""
    startup = os.path.join(RUNTIME, "startup.sh")
    with open(startup, "w") as fh:
        fh.write(f"#!/bin/bash\nenv | grep ^DISPLAY= > {RUNTIME}/env\nexec sleep 3600\n")
    os.chmod(startup, 0o755)
    # Two screens from the start: pre-provisioning is the one road a stock
    # labwc has, and the patched one takes a startup count the same way.
    env = dict(os.environ, WAYLAND_DISPLAY=socket, WLR_BACKENDS="wayland",
               XDG_RUNTIME_DIR=RUNTIME, WLR_RENDERER="pixman", XDG_CONFIG_HOME=config,
               LIBGL_ALWAYS_SOFTWARE="1", WLR_WL_OUTPUTS="2")
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

    def place(self, x: int) -> None:
        """Map the window with its left edge at `x`; the caller polls for it.

        Mapping again is idempotent, and re-issuing the whole placement is what
        lets a poll recover a window a loaded nested session dropped."""
        self.win.map()
        self.win.configure(x=x, y=300)
        self.d.sync()

    def close(self) -> None:
        self.win.destroy()
        self.d.sync()
        self.d.close()


def main() -> "H.Results":
    """Walk a window across the seam and watch both displays' pixels."""
    # A previous run's env dump, log and socket files would be read as this
    # run's; a stale display number can even point at a live foreign server.
    shutil.rmtree(RUNTIME, ignore_errors=True)
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
    socket = pixelflux.ensure_wayland_display(width=DISPLAY[0], height=DISPLAY[1],
                                              render_node="", auto_gpu="", cursor_size=-1)
    left_cap, right_cap = pixelflux.ScreenCapture(), pixelflux.ScreenCapture()
    # The primary's screen carries its view from the start; the second display
    # is a screen of its own beside it.
    default = [o[0] for o in left_cap.list_outputs()]
    res.check("the primary's screen comes with its view on it", default == [0, 1], default)
    res.check("a second display gets a screen of its own",
              left_cap.create_output(2, DISPLAY[0], DISPLAY[1], DISPLAY[0], 0, 1.0))
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
        # The session arranges the screens it opened by its own rule; it is
        # told the capture arrangement the way Selkies tells it.
        pixelflux.ScreenCapture().set_app_screen_layout(
            inner, [(0, 0, DISPLAY[0], DISPLAY[1]), (DISPLAY[0], 0, DISPLAY[0], DISPLAY[1])])
        time.sleep(1.5)
        screens = pixelflux.ScreenCapture().list_app_screens(inner)
        res.check("the session's screens sit side by side",
                  [(x, y) for _n, x, y in screens] == [(0, 0), (DISPLAY[0], 0)], screens)
        patch = Patch(display, WIN_RGB)

        def settled(want, x, deadline=15.0):
            """Poll both displays for the wanted split, re-placing the window
            between samples: encoding is damage-driven, so a fresh frame takes
            a moment, and a loaded nested session can drop a placement."""
            last = (left.coverage(WIN_RGB), right.coverage(WIN_RGB))
            end = time.monotonic() + deadline
            while not want(*last) and time.monotonic() < end:
                time.sleep(0.4)
                patch.place(x)
                last = (left.coverage(WIN_RGB), right.coverage(WIN_RGB))
            return last

        patch.place(300)
        first = settled(lambda l, r: l > PRESENT and r <= PRESENT, 300)
        res.check("a window on the first display shows only there",
                  first[0] > PRESENT and first[1] <= PRESENT, first)

        patch.place(DISPLAY[0] - 250)
        across = settled(lambda l, r: l > PRESENT and r > PRESENT,
                         DISPLAY[0] - 250)
        res.check("a window over the boundary shows on both",
                  across[0] > PRESENT and across[1] > PRESENT, across)

        patch.place(DISPLAY[0] + 400)
        second = settled(lambda l, r: l <= PRESENT and r > PRESENT,
                         DISPLAY[0] + 400)
        res.check("a window past the boundary shows only on the second",
                  second[0] <= PRESENT and second[1] > PRESENT, second)

        # What a drag rides on. The grab is held across the boundary: the host
        # keeps sending the first screen's window motion past its edge, and the
        # nested compositor has to carry its cursor onto the second screen
        # (labwc-seam.patch) rather than clamp it at the first one's last column.
        # The patched labwc announces itself by its control socket; one without
        # the patches clamps by design, so the crossing is only asked of a
        # compositor that can cross.
        if not os.path.exists(os.path.join(RUNTIME, "labwc.sock")):
            res.skip("a held grab crosses the boundary",
                     "this labwc carries no selkies patches and clamps at the boundary")
            res.skip("injected buttons reach the session's clients",
                     "observed only under the held-grab drive")
            obs = None
        elif not (obs := H.WlObs(inner)).ready(20):
            res.skip("a held grab crosses the boundary", "no observer surface")
        else:
            time.sleep(1.0)
            obs.lines.clear()
            left_cap.inject_mouse_move(500.0, 400.0)
            time.sleep(0.4)
            left_cap.inject_mouse_button(272, 1)
            time.sleep(0.3)
            for x in range(500, SPAN[0] - 40, 50):
                left_cap.inject_mouse_move(float(x), 400.0)
                time.sleep(0.03)
            time.sleep(0.6)
            left_cap.inject_mouse_button(272, 0)
            reach, clicks = -1.0, []
            end = time.monotonic() + 10.0
            while time.monotonic() < end:
                seen = [ln["x"] for ln in obs.lines if ln.get("kind") == "ptr_motion"]
                clicks = [ln for ln in obs.lines if ln.get("kind") == "ptr_button"]
                reach = max(seen) if seen else -1.0
                if reach > DISPLAY[0] + 500 and clicks:
                    break
                time.sleep(0.3)
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
