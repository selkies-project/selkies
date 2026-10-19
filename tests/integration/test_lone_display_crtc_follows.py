#!/usr/bin/env python3
"""A desktop that reads the CRTC when told the screen changed finds the final mode.

MATE's panel lays itself out from the CRTCs of the connected outputs, re-read
in an idle callback after GDK reports the screen or its monitors changed
(mate-panel's panel-multimonitor.c). Where the server has no spare outputs, a
lone display is a logical monitor over the one output, whose CRTC shows exactly
that rectangle, and the WebSockets transport both swaps that monitor and sets
the mode. GDK takes the screen's size from its monitors, so a swap ahead of the
mode set reports the new size while the CRTC still has the old mode, and the
mode set that follows changes no monitor, so nothing is reported again: the
panel stayed laid out for the root the session started on, its bottom half off
screen (jupyter-server-proxy#620).

Driven against a private X server started larger than the page, as a session
is, with no spare outputs, as a stock Xvfb has (the images' Xvfb offers them,
unplugged here), by a GDK client that re-reads the CRTCs as the panel does,
through a page's first size and a later resize. Every read must find the CRTC
where GDK's monitor is: a read that finds them apart is the panel laid out for
a mode the screen no longer has.

Usage: python3 tests/integration/test_lone_display_crtc_follows.py
"""
import asyncio
import json
import os
import subprocess
import sys
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(os.path.dirname(TESTS), "src")
sys.path.insert(0, SRC)
sys.path.insert(0, TESTS)
import helpers as H  # noqa: E402
import websockets  # noqa: E402

ROOT = (4096, 2160)
FIRST = (1280, 720)
SECOND = (1024, 768)

# Runs in whichever interpreter has the GTK3 bindings: mate-panel's re-read,
# one idle read of the connected outputs' CRTCs after any screen or monitor
# signal, appended to a file with GDK's monitors at that moment.
PROBE = r"""
import json, sys
sys.path.insert(0, sys.argv[3])
import gi
gi.require_version("Gdk", "3.0"); gi.require_version("Gtk", "3.0")
from gi.repository import Gdk, GLib, Gtk
from selkies.Xlib import display as x11_display
from selkies.Xlib.ext import randr

ready, log = sys.argv[1], sys.argv[2]
x = x11_display.Display()
root = x.screen().root
disp, screen = Gdk.Display.get_default(), Gdk.Screen.get_default()
pending, hooked = [0], set()

def crtcs():
    res = root.xrandr_get_screen_resources_current()
    out = []
    for o in res.outputs:
        oi = randr.get_output_info(x, o, res.config_timestamp)
        if oi.connection != randr.Disconnected and oi.crtc:
            c = x.xrandr_get_crtc_info(oi.crtc, res.config_timestamp)
            out.append([c.x, c.y, c.width, c.height])
    return sorted(out)

def reread():
    pending[0] = 0
    mons = [disp.get_monitor(n).get_geometry() for n in range(disp.get_n_monitors())]
    with open(log, "a") as f:
        f.write(json.dumps({"crtcs": crtcs(),
                            "monitors": sorted([g.x, g.y, g.width, g.height] for g in mons)}) + "\n")
    return False

def changed(*_):
    if not pending[0]:
        pending[0] = GLib.idle_add(reread)
    for n in range(disp.get_n_monitors()):
        m = disp.get_monitor(n)
        if m not in hooked:
            hooked.add(m)
            m.connect("invalidate", changed)

for name in ("size-changed", "monitors-changed"):
    screen.connect(name, changed)
for name in ("monitor-added", "monitor-removed"):
    disp.connect(name, changed)
changed()
open(ready, "w").close()
Gtk.main()
"""


def interpreter_with(probe: str):
    """The first interpreter that runs ``probe``, or None."""
    for cand in dict.fromkeys((sys.executable, "python3", "/usr/bin/python3")):
        if cand and subprocess.run([cand, "-c", probe], capture_output=True).returncode == 0:
            return cand
    return None


def unplug_spare_outputs(display_name: str) -> None:
    """Leave the server as a stock Xvfb is: the images' Xvfb offers spare
    outputs through a ``Connected`` property, which the server takes as the
    capability to lay displays out as outputs of their own."""
    from selkies.Xlib import display as x11_display
    from selkies.Xlib.ext import randr

    d = x11_display.Display(display_name)
    try:
        connected = d.intern_atom("Connected")
        for out in randr.get_screen_resources(d.screen().root).outputs:
            if connected in randr.list_output_properties(d, out).atoms:
                randr.delete_output_property(d, out, connected)
        d.sync()
    finally:
        d.close()


def root_size(display_name: str) -> tuple:
    """The root's size, read on a fresh connection."""
    from selkies.Xlib import display as x11_display
    d = x11_display.Display(display_name)
    try:
        return d.screen().width_in_pixels, d.screen().height_in_pixels
    finally:
        d.close()


def settings_payload() -> str:
    """The first SETTINGS of a page sized ``FIRST``."""
    return "SETTINGS," + json.dumps({
        "displayId": "primary", "initialClientWidth": FIRST[0], "initialClientHeight": FIRST[1],
        "manual_resolution": False, "framerate": 60, "encoder": "h264enc", "scaling_dpi": 96})


def reads(log: str) -> list:
    """The probe's re-reads, oldest first."""
    try:
        with open(log) as f:
            return [json.loads(line) for line in f if line.strip()]
    except OSError:
        return []


async def settle(display_name: str, log: str, size: tuple) -> tuple:
    """Wait for the root to reach ``size`` and the probe's reads to stop.

    Returns:
        The root size reached and the probe's reads.
    """
    deadline = time.time() + 30
    while time.time() < deadline and root_size(display_name) != size:
        await asyncio.sleep(0.2)
    seen, quiet = len(reads(log)), time.time()
    while time.time() - quiet < 2 and time.time() < deadline + 10:
        await asyncio.sleep(0.2)
        if len(reads(log)) != seen:
            seen, quiet = len(reads(log)), time.time()
    return root_size(display_name), reads(log)


def run() -> "H.Results":
    res = H.Results("lone-display-crtc-follows")
    python = interpreter_with(
        "import gi; gi.require_version('Gtk','3.0'); import gi.repository.Gtk")
    if not python:
        res.skip("a desktop reading the CRTC when the screen changes finds the final mode",
                 "no interpreter here can import the GTK3 bindings")
        res.summary()
        return res

    xvfb, display_name = H.private_x_server(*ROOT)
    ready = os.path.join(H.WORKDIR, "crtc-probe-ready")
    log = os.path.join(H.WORKDIR, "crtc-probe.jsonl")
    script = os.path.join(H.WORKDIR, "crtc_probe.py")
    with open(script, "w") as f:
        f.write(PROBE)
    for path in (ready, log):
        if os.path.exists(path):
            os.unlink(path)
    probe = None
    try:
        unplug_spare_outputs(display_name)
        probe = subprocess.Popen([python, script, ready, log, SRC],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
                                 env=dict(os.environ, DISPLAY=display_name))
        deadline = time.time() + 20
        while not os.path.exists(ready) and time.time() < deadline and probe.poll() is None:
            time.sleep(0.1)
        if not os.path.exists(ready):
            res.check("the GDK probe started", False, (probe.stderr.read() or "")[-160:])
            res.summary()
            return res
        H.server_start(mode="websockets", wayland=False, extra_env={"DISPLAY": display_name})

        async def drive():
            uri = f"ws://localhost:{H.PORT}/api/websockets"
            async with websockets.connect(uri, max_size=None) as ws:
                await asyncio.wait_for(ws.recv(), timeout=10)
                await ws.send(settings_payload())
                done = 0
                for label, size in (("a page's first size", FIRST), ("a later resize", SECOND)):
                    if size != FIRST:
                        await ws.send(f"r,{size[0]}x{size[1]},primary")
                    got, seen = await settle(display_name, log, size)
                    want = [[0, 0, size[0], size[1]]]
                    res.check(f"{label} reaches the root", got == size, got)
                    apart = [r for r in seen[done:] if r["crtcs"] != r["monitors"]]
                    res.check(f"{label}: every re-read found the CRTC where GDK's monitor is",
                              not apart, apart[:2])
                    res.check(f"{label}: and the last at the new mode",
                              len(seen) > done and seen[-1]["crtcs"] == want, seen[-1:])
                    done = len(seen)

        asyncio.run(drive())
    finally:
        H.server_stop()
        if probe:
            probe.terminate()
            try:
                probe.wait(timeout=5)
            except subprocess.TimeoutExpired:
                probe.kill()
        H.stop_x_server(xvfb, display_name)
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
