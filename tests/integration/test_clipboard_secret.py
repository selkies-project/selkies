#!/usr/bin/env python3
"""A password manager's copy is read as a secret on every backend.

KeePassXC and KDE offer `x-kde-passwordManagerHint` = `secret` beside a
password they copy, and a toolkit that validates mime types sees the hint only
under a `text/` or `application/` prefix. Each backend is driven the way an
application drives it: an X client that owns CLIPBOARD and answers the
conversions (python-xlib), a data-control source on the capture compositor
(pixelflux's own client, as wl-copy is one), and one on a nested labwc, whose
selection the server reads over data-control. On each the copy reads as its text
alone, marked `SecretText`, and the selection going away afterwards is seen:
the X monitor reports it as an empty secret, the compositor callback delivers
no flavours, and the data-control watch reports no mimes. The callback hands
copies over in the order they were made, so a clear, or a newer copy, is never
followed by an older copy whose source answered late.
Usage: python3 tests/integration/test_clipboard_secret.py
"""
import asyncio
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.path.insert(0, TESTS)
sys.argv = ["selkies"]

import helpers as H  # noqa: E402
from selkies.Xlib import X, display as xdisp  # noqa: E402
from selkies.Xlib.protocol import event as xevent, request as xrequest  # noqa: E402
from selkies.input_handler import (  # noqa: E402
    CLIPBOARD_SECRET_HINTS, SecretText, WebRTCInput, _X11ClipboardMonitor)

PASSWORD = "correct horse battery staple"


class XOwner:
    """An X client owning CLIPBOARD with `(target, bytes)` offers, as a
    password manager does, served from a thread until `clear`."""

    def __init__(self, display_name: str, offers: list) -> None:
        self.d = xdisp.Display(display_name)
        screen = self.d.screen()
        self.win = screen.root.create_window(0, 0, 1, 1, 0, screen.root_depth,
                                             window_class=X.InputOutput)
        self.clip = self.d.get_atom("CLIPBOARD")
        self.targets = self.d.get_atom("TARGETS")
        self.offers = [(self.d.get_atom(name), data) for name, data in offers]
        self.stop = threading.Event()
        self.win.set_selection_owner(self.clip, X.CurrentTime)
        self.d.flush()
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()

    def _serve(self) -> None:
        while not self.stop.is_set():
            if not self.d.pending_events():
                time.sleep(0.005)
                continue
            ev = self.d.next_event()
            if not isinstance(ev, xevent.SelectionRequest):
                continue
            prop = ev.property
            if ev.target == self.targets:
                ev.requestor.change_property(prop, self.d.get_atom("ATOM"), 32,
                                             [self.targets] + [a for a, _ in self.offers])
            elif any(a == ev.target for a, _ in self.offers):
                data = next(d for a, d in self.offers if a == ev.target)
                ev.requestor.change_property(prop, ev.target, 8, data)
            else:
                prop = X.NONE
            ev.requestor.send_event(xevent.SelectionNotify(
                time=ev.time, requestor=ev.requestor, selection=ev.selection,
                target=ev.target, property=prop), propagate=False)
            self.d.flush()

    def clear(self) -> None:
        """Give CLIPBOARD up, as KeePassXC does when its timeout runs out."""
        self.stop.set()
        self.thread.join(2.0)
        xrequest.SetSelectionOwner(display=self.d.display, window=X.NONE,
                                   selection=self.clip, time=X.CurrentTime)
        self.d.flush()
        self.d.close()


def x11_reads(res: H.Results, display: str) -> None:
    reader = _X11ClipboardMonitor(display)
    try:
        for hint in CLIPBOARD_SECRET_HINTS:
            owner = XOwner(display, [("UTF8_STRING", PASSWORD.encode()),
                                     ("text/plain;charset=utf-8", PASSWORD.encode()), (hint, b"secret")])
            try:
                data, mime = reader.read(use_binary=True)
                res.check(f"x11: an owner offering {hint} reads as its text, marked secret",
                          isinstance(data, SecretText) and data == PASSWORD and mime == "text/plain",
                          (type(data).__name__, mime))
            finally:
                owner.clear()
        owner = XOwner(display, [("UTF8_STRING", b"ordinary")])
        try:
            data, _ = reader.read(use_binary=True)
            res.check("x11: an owner without the hint reads as ordinary text",
                      data == "ordinary" and not isinstance(data, SecretText), repr(data))
        finally:
            owner.clear()

        # The server writing the secret back offers the hint with it.
        writer = _X11ClipboardMonitor(display)
        try:
            writer.offer([("text/plain", PASSWORD.encode())]
                         + [(hint, b"secret") for hint in CLIPBOARD_SECRET_HINTS])
            data, _ = reader.read(use_binary=True)
            res.check("x11: a secret the server offers reads back as one",
                      isinstance(data, SecretText) and data == PASSWORD, repr(type(data)))
        finally:
            writer.close()
    finally:
        reader.close()


def x11_monitor(res: H.Results, display: str) -> None:
    """The outbound monitor on a real X server: the secret goes out marked, and
    the owner giving the selection up goes out as an empty secret."""
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = False
    h.enable_clipboard = "true"
    h.enable_binary_clipboard = "true"
    h._clipboard_monitor_active = False
    h.clipboard_running = False
    h._clipboard_last_bytes = None
    h._clipboard_self_write = None
    h._bg_tasks = set()
    h._clipboard_has_consumers = lambda: True
    h._x11_clipboard_monitor = _X11ClipboardMonitor(display)
    sent: list = []

    async def monitor_async():
        return h._x11_clipboard_monitor
    h._ensure_x11_clipboard_monitor_async = monitor_async

    async def on_read(data, mime):
        sent.append(data)
    h.on_clipboard_read = on_read

    async def run() -> None:
        task = asyncio.create_task(h.start_clipboard())
        await asyncio.sleep(0.5)
        owner = XOwner(display, [("UTF8_STRING", PASSWORD.encode()), (CLIPBOARD_SECRET_HINTS[0], b"secret")])
        await asyncio.sleep(1.0)
        res.check("x11 monitor: the password manager's copy goes out marked",
                  sent[-1:] == [PASSWORD] and isinstance(sent[-1], SecretText), sent)
        started = time.monotonic()
        owner.clear()
        while time.monotonic() - started < 5 and not (sent and sent[-1] == ""):
            await asyncio.sleep(0.05)
        res.check("x11 monitor: its giving the selection up goes out as an empty secret",
                  sent[-1:] == [""] and isinstance(sent[-1], SecretText),
                  f"{sent} after {time.monotonic() - started:.2f} s")
        h.clipboard_running = False
        await asyncio.wait_for(task, 5.0)

    try:
        asyncio.run(run())
    finally:
        h.stop_clipboard()


def wait_for(items: list, count: int, seconds: float = 5.0) -> bool:
    deadline = time.time() + seconds
    while time.time() < deadline and len(items) < count:
        time.sleep(0.05)
    return len(items) >= count


def slow_source(socket_path: str, delay: float, text: bytes) -> None:
    """A data-control source that takes `delay` seconds to answer a paste, owning
    the selection until another client takes it."""
    from pywayland.client import Display
    from pywayland.protocol.wayland import WlSeat
    from pywayland.protocol.ext_data_control_v1 import ExtDataControlManagerV1
    display = Display(socket_path)
    display.connect()
    found = {}

    def on_global(registry, name, interface, version):
        if interface == "wl_seat" and "seat" not in found:
            found["seat"] = registry.bind(name, WlSeat, 1)
        elif interface == "ext_data_control_manager_v1":
            found["manager"] = registry.bind(name, ExtDataControlManagerV1, 1)

    registry = display.get_registry()
    registry.dispatcher["global"] = on_global
    display.roundtrip()
    device = found["manager"].get_data_device(found["seat"])
    source = found["manager"].create_data_source()
    source.offer("text/plain;charset=utf-8")
    canceled = threading.Event()

    def answer(fd: int) -> None:
        time.sleep(delay)
        try:
            os.write(fd, text)
        finally:
            os.close(fd)

    source.dispatcher["send"] = lambda _s, _mime, fd: threading.Thread(
        target=answer, args=(fd,), daemon=True).start()
    source.dispatcher["cancelled"] = lambda _s: canceled.set()
    device.set_selection(source)
    display.roundtrip()
    while not canceled.is_set():
        display.dispatch(block=True)
    time.sleep(delay + 0.5)
    display.disconnect()


def compositor_block(res: H.Results) -> None:
    """A data-control source on the capture compositor, read through its callback."""
    import pixelflux
    runtime = os.environ.get("XDG_RUNTIME_DIR") or "/tmp"
    os.environ.setdefault("XDG_RUNTIME_DIR", runtime)
    socket_name = pixelflux.ensure_wayland_display(width=800, height=600)
    capture = pixelflux.ScreenCapture()
    deliveries: list = []
    capture.set_clipboard_callback(lambda entries: deliveries.append(
        [(mime, bytes(data)) for mime, data in entries]))
    source = pixelflux.ScreenCapture()
    for hint in CLIPBOARD_SECRET_HINTS:
        seen = len(deliveries)
        source.clipboard_write_app(socket_name, [("text/plain;charset=utf-8", PASSWORD.encode()),
                                                 (hint, b"secret")])
        got = deliveries[-1] if wait_for(deliveries, seen + 1) else []
        data, mime = WebRTCInput._native_clipboard_payload(got, True)
        res.check(f"compositor: a source offering {hint} is delivered with it, and reads as a secret",
                  isinstance(data, SecretText) and data == PASSWORD, [m for m, _ in got])
    seen = len(deliveries)
    source.clipboard_clear_app(socket_name)
    res.check("compositor: a source clearing the selection is delivered as no flavours",
              wait_for(deliveries, seen + 1) and deliveries[-1] == [], deliveries[seen:])

    # A source slow to answer (Firefox re-encoding an image takes seconds) and a
    # copy made meanwhile: the newer copy is the selection, so it has to be the
    # last delivery, never the older one whose bytes arrive after it.
    seen = len(deliveries)
    slow = threading.Thread(target=slow_source, args=(
        os.path.join(runtime, socket_name), 1.5, b"the older copy"), daemon=True)
    slow.start()
    time.sleep(0.5)
    source.clipboard_write_app(socket_name, [("text/plain;charset=utf-8", b"the newer copy")])
    slow.join(10.0)
    time.sleep(0.5)
    got = [d[0][1] if d else b"" for d in deliveries[seen:]]
    res.check("compositor: a slow copy overtaken by a newer one is not delivered after it",
              got[-1:] == [b"the newer copy"], got)


def boot_labwc(runtime: str) -> tuple:
    env = dict(os.environ, XDG_RUNTIME_DIR=runtime, WLR_BACKENDS="headless",
               WLR_LIBINPUT_NO_DEVICES="1", WLR_RENDERER="pixman")
    env.pop("WAYLAND_DISPLAY", None)
    env.pop("DISPLAY", None)
    log = open(os.path.join(runtime, "labwc.log"), "w")
    proc = H.spawn(["labwc"], env=env, stdout=log, stderr=subprocess.STDOUT)
    for _ in range(80):
        socket = next((n for n in sorted(os.listdir(runtime))
                       if n.startswith("wayland-") and not n.endswith(".lock")), "")
        if socket:
            return proc, socket
        time.sleep(0.25)
    return proc, ""


def nested_block(res: H.Results) -> None:
    """A data-control source on a nested labwc, read the way the server reads an
    app compositor's selection, and watched."""
    if shutil.which("labwc") is None:
        res.skip("nested: a data-control source on labwc", "labwc is not installed")
        return
    import pixelflux
    runtime = tempfile.mkdtemp(prefix="clipsecret-")
    os.chmod(runtime, 0o700)
    saved = os.environ.get("XDG_RUNTIME_DIR")
    os.environ["XDG_RUNTIME_DIR"] = runtime
    proc, socket = boot_labwc(runtime)
    try:
        if not socket:
            res.check("nested: labwc came up", False, H.tail(os.path.join(runtime, "labwc.log")))
            return
        client = pixelflux.ScreenCapture()
        changes: list = []
        client.clipboard_watch_app(socket, lambda mimes: changes.append(list(mimes)))
        h = WebRTCInput.__new__(WebRTCInput)
        h.wayland_input = client
        h._app_wayland_display = lambda: socket
        h._app_clip_read_failure = None
        for hint in CLIPBOARD_SECRET_HINTS:
            client.clipboard_write_app(socket, [("text/plain;charset=utf-8", PASSWORD.encode()),
                                                ("text/plain", PASSWORD.encode()), (hint, b"secret")])
            data, mime = asyncio.run(h._app_clipboard_read(use_binary=True))
            res.check(f"nested: a source offering {hint} reads as its text, marked secret",
                      isinstance(data, SecretText) and data == PASSWORD and mime == "text/plain",
                      (type(data).__name__, mime))
        client.clipboard_write_app(socket, [("text/plain", b"ordinary")])
        data, _ = asyncio.run(h._app_clipboard_read(use_binary=True))
        res.check("nested: a source without the hint reads as ordinary text",
                  data == "ordinary" and not isinstance(data, SecretText), repr(data))
        seen = len(changes)
        client.clipboard_clear_app(socket)
        res.check("nested: the watch reports a cleared selection as no mimes",
                  wait_for(changes, seen + 1) and changes[-1] == [], changes[seen:])
        client.clipboard_unwatch_app(socket)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except Exception:
            proc.kill()
        if saved is None:
            os.environ.pop("XDG_RUNTIME_DIR", None)
        else:
            os.environ["XDG_RUNTIME_DIR"] = saved
        shutil.rmtree(runtime, ignore_errors=True)


def main() -> H.Results:
    res = H.Results("clip-secret")
    proc, display = H.private_x_server()
    try:
        x11_reads(res, display)
        x11_monitor(res, display)
    finally:
        H.stop_x_server(proc, display)
    nested_block(res)
    compositor_block(res)
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
