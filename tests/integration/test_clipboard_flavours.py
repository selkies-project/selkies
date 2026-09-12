#!/usr/bin/env python3
"""The session's own clipboard holds every flavour of one copy.

An application pasting from the session asks for the flavour it wants, so the
offer has to answer each one with its own payload rather than with whichever
was picked for all of them. Both backends are driven the way an application
drives them: an X client converting the selection target by target, and a
Wayland client asking the compositor for a mime at a time.
Usage: python3 tests/integration/test_clipboard_flavours.py
"""
import os
import subprocess
import sys
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.path.insert(0, TESTS)
sys.argv = ["selkies"]

import helpers as H  # noqa: E402
from selkies.Xlib import X, display as xdisp  # noqa: E402
from selkies.input_handler import (  # noqa: E402
    CLIPBOARD_FLAVOURS_MIME, _X11ClipboardMonitor, clipboard_flavours)

HTML, PLAIN = b"<b>rich</b>", b"plain text"


def convert(display_name: str, target: str):
    """One selection conversion on a connection of its own, as a paste does."""
    d = xdisp.Display(display_name)
    try:
        screen = d.screen()
        win = screen.root.create_window(0, 0, 1, 1, 0, screen.root_depth,
                                        window_class=X.InputOutput)
        prop = d.get_atom("SELKIES_PASTE")
        win.convert_selection(d.get_atom("CLIPBOARD"), d.get_atom(target), prop, X.CurrentTime)
        d.flush()
        deadline = time.time() + 5
        while time.time() < deadline:
            if d.pending_events():
                event = d.next_event()
                if event.type == X.SelectionNotify:
                    if event.property == X.NONE:
                        return None
                    value = win.get_full_property(prop, X.AnyPropertyType)
                    return None if value is None else value.value
            time.sleep(0.01)
        return None
    finally:
        d.close()


def x11_block(res: H.Results) -> None:
    proc, display = H.private_x_server()
    try:
        monitor = _X11ClipboardMonitor(display)
        res.check("x11: the offer is taken", monitor.offer(
            [("text/html", HTML), ("text/plain", PLAIN)]))
        targets = sorted(monitor._d.get_atom_name(a) for a in (convert(display, "TARGETS") or []))
        res.check("x11: every flavour is advertised, text under each of its names",
                  {"text/html", "UTF8_STRING", "text/plain", "STRING", "TEXT"} <= set(targets), targets)
        html = convert(display, "text/html")
        plain = convert(display, "UTF8_STRING")
        res.check("x11: each flavour is served as itself",
                  (bytes(html), bytes(plain)) == (HTML, PLAIN), (html, plain))
        # A second connection reads what the first offers, which is the shape a
        # copy out of the session takes: one envelope carrying both flavours.
        reader = _X11ClipboardMonitor(display)
        data, mime = reader.read(use_binary=False)
        res.check("x11: a rich copy reads back as one envelope",
                  mime == CLIPBOARD_FLAVOURS_MIME, mime)
        res.check("x11: the envelope carries the markup and the text beneath it",
                  clipboard_flavours(data) == [("text/html", HTML), ("text/plain", PLAIN)],
                  data)
    finally:
        H.stop_x_server(proc, display)


def wayland_block(res: H.Results) -> None:
    import pixelflux
    runtime = os.environ.get("XDG_RUNTIME_DIR") or "/tmp"
    os.environ.setdefault("XDG_RUNTIME_DIR", runtime)
    socket_name = pixelflux.ensure_wayland_display(width=800, height=600)
    capture = pixelflux.ScreenCapture()
    capture.set_clipboard([("text/html", HTML), ("text/plain", PLAIN)])
    time.sleep(1.0)
    env = {**os.environ, "WAYLAND_DISPLAY": socket_name, "XDG_RUNTIME_DIR": runtime}

    def paste(*args):
        result = subprocess.run(["wl-paste", *args], env=env, capture_output=True,
                                text=True, timeout=15)
        return (result.stdout or result.stderr).strip()

    offered = set(paste("-l").split())
    res.check("wayland: every flavour is advertised, text under each of its names",
              {"text/html", "text/plain", "UTF8_STRING", "STRING", "TEXT"} <= offered, sorted(offered))
    res.check("wayland: each flavour is served as itself",
              (paste("-t", "text/html", "-n"), paste("-t", "text/plain", "-n"))
              == (HTML.decode(), PLAIN.decode()))


def main() -> H.Results:
    res = H.Results("clip-flavours")
    x11_block(res)
    wayland_block(res)
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
