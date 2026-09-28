#!/usr/bin/env python3
"""A monitor-set change reaches the toolkits already running on the session.

RRSetMonitor emits no RandR event of its own. The server sends core
ConfigureNotify on the root, and GTK3 discards those wherever RandR 1.3 is
present, re-reading its monitor list only for RRScreenChangeNotify or RRNotify
(`_gdk_x11_screen_size_changed`). A swap that resizes the framebuffer is
carried by the resize; one that does not — a display added inside a root
already large enough, a display removed, a display moved to the other side of
the primary — reaches a running desktop only because the publish announces it
on an output property.

Driven on a root fixed at the union of every layout below, so no swap here can
change the framebuffer size and nothing but the announcement can prompt the
re-read. Read back from the wire, and from GDK's own monitor list wherever an
interpreter with GTK3 bindings is available: the wire proves the event is sent,
GDK proves it is the one a desktop acts on. Qt re-reads the monitors on the
root's ConfigureNotify but announces a new geometry only for a monitor that is
exactly one CRTC, so a display that moves or resizes beside another reaches a
running Qt client only as a monitor of a new name; that is read from the
screens of a Qt 6 client wherever an interpreter with PyQt6 is available.
"""
import asyncio
import json
import os
import subprocess
import sys
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)
import helpers as H  # noqa: E402

PRIMARY = {"x": 0, "y": 0, "w": 1024, "h": 640}
ONE = {"primary": dict(PRIMARY)}
RIGHT = {"primary": dict(PRIMARY), "display2": {"x": 1024, "y": 0, "w": 640, "h": 480}}
LEFT = {"primary": {"x": 640, "y": 0, "w": 1024, "h": 640},
        "display2": {"x": 0, "y": 0, "w": 640, "h": 480}}
UNION = (1664, 640)
# One point inside each rectangle of RIGHT, well clear of the seam.
IN_PRIMARY = (100, 100)
IN_DISPLAY2 = (1400, 200)

# Runs in whichever interpreter has the GTK3 bindings: reports the monitor list
# a client that was already running sees once the swap has landed.
GDK_PROBE = r"""
import json, sys, gi
gi.require_version("Gdk", "3.0"); gi.require_version("Gtk", "3.0")
from gi.repository import Gdk, Gtk, GLib

d = Gdk.Display.get_default()
ready, pts = sys.argv[1], [tuple(int(v) for v in a.split(",")) for a in sys.argv[2:]]

def state():
    at = []
    for x, y in pts:
        g = d.get_monitor_at_point(x, y).get_geometry()
        at.append([g.x, g.y, g.width, g.height])
    return {"n": d.get_n_monitors(), "at": at}

w = Gtk.Window(); w.set_default_size(50, 50); w.show_all()
before = state()
open(ready, "w").close()
GLib.timeout_add(4000, lambda: (print(json.dumps({"before": before, "after": state()})),
                                Gtk.main_quit())[1])
Gtk.main()
"""


# Runs in whichever interpreter has PyQt6: the screens a Qt client that was
# already running has once the swap has landed.
QT_PROBE = r"""
import json, sys
from PyQt6.QtCore import QTimer
from PyQt6.QtGui import QGuiApplication

app = QGuiApplication(sys.argv[:1])
ready = sys.argv[1]

def state():
    return sorted([s.geometry().x(), s.geometry().y(), s.geometry().width(), s.geometry().height()]
                  for s in app.screens())

before = state()
QTimer.singleShot(0, lambda: open(ready, "w").close())
QTimer.singleShot(4000, lambda: (print(json.dumps({"before": before, "after": state()})), app.quit()))
app.exec()
"""


def interpreter_with(probe: str):
    """The first interpreter that runs ``probe``, or None.

    The suites run on whatever interpreter the capture stack is installed for,
    which is rarely the one carrying a distribution's PyGObject or PyQt; the
    system interpreter usually is.
    """
    for cand in dict.fromkeys((sys.executable, "python3", "/usr/bin/python3")):
        if cand and subprocess.run([cand, "-c", probe], capture_output=True).returncode == 0:
            return cand
    return None


def qt_across(python, display_name, swap, res, label):
    """Run a Qt client through ``swap`` and return its before/after screens."""
    ready = os.path.join(H.WORKDIR, "qt-probe-ready")
    script = os.path.join(H.WORKDIR, "qt_probe.py")
    with open(script, "w") as f:
        f.write(QT_PROBE)
    if os.path.exists(ready):
        os.unlink(ready)
    env = dict(os.environ, DISPLAY=display_name, QT_QPA_PLATFORM="xcb")
    proc = subprocess.Popen([python, script, ready], stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, env=env)
    deadline = time.time() + 20
    while not os.path.exists(ready) and time.time() < deadline:
        if proc.poll() is not None:
            break
        time.sleep(0.1)
    swap()
    out, err = proc.communicate(timeout=30)
    try:
        return json.loads(out.strip().splitlines()[-1])
    except Exception:
        res.check(f"{label}: the Qt probe reported its screens", False,
                  (out or err or "no output")[-160:])
        return None


def publish(layouts):
    """Swap the live selkies-* set to ``layouts`` through the shipped path."""
    import selkies.display_utils_xrandr as DX
    DX._sync_replace_selkies_monitors(layouts)


def primary_output(display_name):
    """The RandR primary output of the server, 0 for none."""
    from selkies.Xlib import display as x11_display
    from selkies.Xlib.ext import randr

    d = x11_display.Display(display_name)
    try:
        return int(randr.get_output_primary(d.screen().root).output)
    finally:
        d.close()


def randr_events(display_name, during, seconds=2.0):
    """RandR events the server sends a bystander while ``during`` runs.

    A connection of its own, selecting what GTK3 selects, so what is counted is
    what a desktop would have been woken by.
    """
    from selkies.Xlib import display as x11_display
    from selkies.Xlib.ext import randr

    d = x11_display.Display(display_name)
    try:
        randr.select_input(
            d.screen().root,
            randr.RRScreenChangeNotifyMask | randr.RRCrtcChangeNotifyMask
            | randr.RROutputChangeNotifyMask | randr.RROutputPropertyNotifyMask)
        d.sync()
        while d.pending_events():
            d.next_event()
        during()
        names, deadline = [], time.time() + seconds
        while time.time() < deadline:
            while d.pending_events():
                names.append(type(d.next_event()).__name__)
            if names:
                break
            time.sleep(0.05)
        return names
    finally:
        d.close()


def gdk_across(python, display_name, swap, res, label):
    """Run a GDK client through ``swap`` and return its before/after monitors.

    The client is started first and probed after, which is the case that
    matters: a desktop reads the monitor set once at startup and thereafter
    only when the server tells it to.
    """
    ready = os.path.join(H.WORKDIR, "gdk-probe-ready")
    script = os.path.join(H.WORKDIR, "gdk_probe.py")
    with open(script, "w") as f:
        f.write(GDK_PROBE)
    if os.path.exists(ready):
        os.unlink(ready)
    env = dict(os.environ, DISPLAY=display_name)
    proc = subprocess.Popen(
        [python, script, ready, "%d,%d" % IN_PRIMARY, "%d,%d" % IN_DISPLAY2],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
    deadline = time.time() + 20
    while not os.path.exists(ready) and time.time() < deadline:
        if proc.poll() is not None:
            break
        time.sleep(0.1)
    swap()
    out, err = proc.communicate(timeout=30)
    try:
        return json.loads(out.strip().splitlines()[-1])
    except Exception:
        res.check(f"{label}: the GDK probe reported its monitors", False,
                  (out or err or "no output")[-160:])
        return None


def run() -> H.Results:
    res = H.Results("monitor-change-announced")
    xvfb, display_name = H.private_x_server(*UNION)
    os.environ["DISPLAY"] = display_name
    try:
        import selkies.display_utils as D
        import selkies.display_utils_xrandr as DX

        publish(ONE)
        events = randr_events(display_name, lambda: publish(RIGHT))
        res.check("adding a display sends a bystander a RandR event",
                  any("Property" in n or "ScreenChange" in n for n in events), events)

        publish(ONE)
        events = randr_events(display_name, lambda: publish(dict(ONE)))
        res.check("a swap matching the live set sends nothing", not events, events)

        # Qt takes any monitor listing the primary output for the primary
        # screen, so with two displays sharing the output none is primary
        res.check("a single display keeps the output primary",
                  primary_output(display_name) != 0, primary_output(display_name))
        publish(RIGHT)
        res.check("two displays over one output leave no output primary",
                  primary_output(display_name) == 0, primary_output(display_name))
        publish(ONE)
        res.check("and back to one display the output is primary again",
                  primary_output(display_name) != 0, primary_output(display_name))

        root = D._module_display().screen().root.get_geometry()
        res.check("the root never changed size, so nothing else could prompt a re-read",
                  (root.width, root.height) == UNION, (root.width, root.height))

        python = interpreter_with("from PyQt6.QtGui import QGuiApplication")
        if not python:
            res.skip("a running Qt client follows displays that move or resize",
                     "no interpreter here can import PyQt6")
        else:
            publish(RIGHT)
            taller = {"primary": dict(PRIMARY), "display2": {**RIGHT["display2"], "h": 600}}
            seen = qt_across(python, display_name, lambda: publish(taller), res, "resize")
            if seen:
                res.check("a display resized beside another reaches a running Qt client",
                          seen["after"] == [[0, 0, 1024, 640], [1024, 0, 640, 600]], seen)
            seen = qt_across(python, display_name, lambda: publish(LEFT), res, "move")
            if seen:
                res.check("displays that trade sides reach a running Qt client",
                          seen["after"] == [[0, 0, 640, 480], [640, 0, 1024, 640]], seen)
            publish(ONE)

        python = interpreter_with(
            "import gi; gi.require_version('Gtk','3.0'); import gi.repository.Gtk")
        if not python:
            res.skip("a running toolkit follows the swap",
                     "no interpreter here can import the GTK3 bindings")
            res.summary()
            return res

        publish(ONE)
        seen = gdk_across(python, display_name, lambda: publish(RIGHT), res, "add")
        if seen:
            res.check("a display added without a resize reaches a running toolkit",
                      seen["before"]["n"] == 1 and seen["after"]["n"] == 2, seen)
            res.check("and the added rectangle is a monitor of its own",
                      seen["after"]["at"][0] != seen["after"]["at"][1], seen["after"])

        seen = gdk_across(python, display_name, lambda: publish(ONE), res, "remove")
        if seen:
            res.check("a removed display stops being a monitor",
                      seen["before"]["n"] == 2 and seen["after"]["n"] == 1, seen)

        publish(RIGHT)
        seen = gdk_across(python, display_name, lambda: publish(LEFT), res, "move")
        if seen:
            res.check("a display moved to the other side carries its rectangle",
                      seen["after"]["at"][0] == [0, 0, 640, 480], seen["after"])

        # Teardown reaches the final set over its own route, so it announces
        # for itself rather than through the grab-protected replace.
        seen = gdk_across(python, display_name,
                          lambda: asyncio.run(DX.clear_selkies_monitors()), res, "clear")
        if seen:
            res.check("clearing every display leaves the toolkit one monitor",
                      seen["before"]["n"] == 2 and seen["after"]["n"] == 1, seen)
    finally:
        H.stop_x_server(xvfb, display_name)
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
