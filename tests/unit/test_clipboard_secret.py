#!/usr/bin/env python3
"""A password manager's copy leaves the session marked secret, on every rung.

KeePassXC and KDE offer `x-kde-passwordManagerHint` = `secret` beside a
password they copy, so that no clipboard history keeps it; a toolkit that
validates mime types sees the hint only under a `text/` or `application/`
prefix. Each reader (the X11 monitor, the capture compositor's callback, the
app compositor's data-control client) reads such a copy as its text alone,
marked `SecretText`; both transports carry the mark to the clients; the monitor
tells them when the session's clipboard lets the secret go; and the server
offers the hint again when it writes the secret back. A viewer's link carries
no announcement at all, since a viewer's page never takes the session's
clipboard. The readers here are
driven through fakes of the APIs they call; tests/integration's
test_clipboard_secret.py drives the real X server and compositors.
"""
import asyncio
import json
import os
import sys
import threading
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import webrtc_engine as rte  # noqa: E402
from selkies import websockets_mode as wsm  # noqa: E402
from selkies.input_handler import (  # noqa: E402
    CLIPBOARD_SECRET_HINTS, SecretText, WebRTCInput, _X11ClipboardMonitor)

results = []
PASSWORD = "correct horse battery staple"
# The spellings password managers offer the hint under, written out rather than
# read from the server's CLIPBOARD_SECRET_HINTS: the checks hold the server to
# them, and CodeQL takes whatever is read from that table for a secret.
HINTS = ("x-kde-passwordManagerHint", "text/x-kde-passwordManagerHint",
         "application/x-kde-passwordManagerHint")


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [clip-secret] {label}  {str(detail)[:160]}", flush=True)


def is_secret(data, text: str) -> bool:
    return isinstance(data, SecretText) and data == text


def shown(data) -> str:
    """Clipboard data as a check prints it: its type and length, never its text."""
    return "None" if data is None else f"{type(data).__name__} of {len(data)}"


def x11_reads() -> None:
    """The X11 monitor's read, its conversions answered by a table."""
    atoms = {name: n for n, name in enumerate(
        ["TARGETS", "UTF8_STRING", "text/plain;charset=utf-8", "STRING", "text/html",
         "text/uri-list", "image/png", *HINTS, *CLIPBOARD_SECRET_HINTS], start=100)}
    monitor = _X11ClipboardMonitor.__new__(_X11ClipboardMonitor)
    monitor._targets = atoms["TARGETS"]
    monitor._image_targets = [(atoms["image/png"], "image/png")]
    monitor._html_atom = atoms["text/html"]
    monitor._text_targets = [(atoms[t], t) for t in ("UTF8_STRING", "text/plain;charset=utf-8", "STRING")]
    monitor._uri_list_atom = atoms["text/uri-list"]
    monitor._secret_hint_atoms = [atoms[h] for h in CLIPBOARD_SECRET_HINTS]

    def owner(offer: dict):
        def convert(atom):
            if atom == atoms["TARGETS"]:
                return [atoms["TARGETS"]] + [atoms[t] for t in offer], 32
            name = next(n for n, a in atoms.items() if a == atom)
            return (offer[name], 8) if name in offer else None
        monitor._convert_and_wait = convert

    for hint in HINTS:
        owner({"UTF8_STRING": PASSWORD.encode(), hint: b"secret"})
        data, mime = monitor.read(use_binary=True)
        check(f"x11: a copy offering {hint} reads as its text, marked secret",
              is_secret(data, PASSWORD) and mime == "text/plain", (type(data).__name__, mime))
    owner({"UTF8_STRING": PASSWORD.encode(), "image/png": b"\x89PNG", "text/html": b"<b>x</b>",
           HINTS[0]: b"secret"})
    data, mime = monitor.read(use_binary=True)
    check("x11: a secret copy reads as its text alone, whatever else it offers",
          is_secret(data, PASSWORD) and mime == "text/plain", mime)
    owner({"UTF8_STRING": b"", HINTS[0]: b"secret"})
    data, mime = monitor.read(use_binary=False)
    check("x11: an empty secret reads as an empty one", is_secret(data, ""), shown(data))
    owner({"UTF8_STRING": b"hello", HINTS[0]: b"public"})
    data, mime = monitor.read(use_binary=False)
    check("x11: a hint valued anything but secret leaves the text ordinary",
          data == "hello" and not isinstance(data, SecretText), shown(data))
    owner({"UTF8_STRING": b"hello"})
    data, mime = monitor.read(use_binary=False)
    check("x11: an ordinary copy reads as before", data == "hello" and not isinstance(data, SecretText))


def native_reads() -> None:
    """The capture compositor's flavours, as its callback delivers them."""
    payload = WebRTCInput._native_clipboard_payload
    for hint in HINTS:
        data, mime = payload([("text/plain;charset=utf-8", PASSWORD.encode()), (hint, b"secret")], True)
        check(f"compositor: a copy offering {hint} reads as its text, marked secret",
              is_secret(data, PASSWORD) and mime == "text/plain", repr(type(data)))
    data, _ = payload([(HINTS[1], b"secret")], True)
    check("compositor: a secret whose text is empty reads as an empty one", is_secret(data, ""), shown(data))
    data, _ = payload([("text/plain", b"hello"), (HINTS[0], b"no")], True)
    check("compositor: a hint valued anything but secret leaves the text ordinary",
          data == "hello" and not isinstance(data, SecretText), shown(data))
    check("compositor: a cleared selection reads as nothing", payload([], True) == (None, None))


class FakeAppCompositor:
    """The pixelflux data-control ABI over a selection of `(mime, bytes)`."""

    def __init__(self, offer: dict) -> None:
        self.offer = offer
        self.reads: list = []

    def clipboard_types_app(self, display: str) -> list:
        return list(self.offer)

    def clipboard_read_app(self, display: str, mime: str):
        self.reads.append(mime)
        return self.offer.get(mime)


async def app_reads() -> None:
    """The app compositor's selection, read over data-control."""
    h = WebRTCInput.__new__(WebRTCInput)
    h._app_wayland_display = lambda: "wayland-2"
    h._app_clip_read_failure = None
    for hint in HINTS:
        h.wayland_input = FakeAppCompositor({"image/png": b"\x89PNG", "text/plain": PASSWORD.encode(),
                                             hint: b"secret"})
        data, mime = await h._app_clipboard_read(use_binary=True)
        check(f"data-control: a copy offering {hint} reads as its text alone, marked secret",
              is_secret(data, PASSWORD) and "image/png" not in h.wayland_input.reads,
              h.wayland_input.reads)
    h.wayland_input = FakeAppCompositor({"text/plain": b"hello"})
    data, _ = await h._app_clipboard_read(use_binary=True)
    check("data-control: an ordinary copy costs no hint read and reads as before",
          data == "hello" and not isinstance(data, SecretText)
          and h.wayland_input.reads == ["text/plain"], h.wayland_input.reads)


class FakeCompositor:
    """pixelflux's compositor clipboard callback, delivering from a foreign
    thread; no entries is a selection a client cleared."""

    def __init__(self) -> None:
        self.callback = None
        self.offered: list = []

    def set_clipboard_callback(self, callback) -> None:
        self.callback = callback

    def deliver(self, *entries) -> None:
        t = threading.Thread(target=self.callback, args=(list(entries),))
        t.start()
        t.join(2.0)

    def set_clipboard(self, entries) -> None:
        self.offered = list(entries)


def handler() -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = True
    h.wayland_input = FakeCompositor()
    h.enable_clipboard = "true"
    h.enable_binary_clipboard = "true"
    h._clipboard_monitor_active = False
    h.clipboard_running = False
    h._clipboard_last_bytes = None
    h._clipboard_self_write = None
    h._bg_tasks = set()
    h._x11_clipboard_monitor = None
    h._app_wl_display_cached = None
    h._has_separate_app_compositor = lambda: False
    h._app_wayland_display = lambda: "wayland-1"
    h._clipboard_has_consumers = lambda: True
    h.sent: list = []

    async def no_monitor():
        return None
    h._ensure_x11_clipboard_monitor_async = no_monitor

    async def on_read(data, mime):
        h.sent.append(data)
    h.on_clipboard_read = on_read
    return h


async def monitor_loop() -> None:
    """The outbound monitor retracts a secret the session's clipboard let go."""
    h = handler()
    comp = h.wayland_input
    task = asyncio.create_task(h.start_clipboard())
    await asyncio.sleep(0.3)
    comp.deliver(("text/plain", PASSWORD.encode()), (HINTS[0], b"secret"))
    await asyncio.sleep(0.3)
    check("monitor: a secret copy reaches the clients marked",
          len(h.sent) == 1 and is_secret(h.sent[0], PASSWORD), [shown(d) for d in h.sent])
    comp.deliver()
    await asyncio.sleep(0.3)
    check("monitor: the selection cleared after it goes out as an empty secret",
          len(h.sent) == 2 and is_secret(h.sent[1], ""), [shown(d) for d in h.sent])
    comp.deliver(("text/plain", b"ordinary"))
    await asyncio.sleep(0.3)
    comp.deliver()
    await asyncio.sleep(0.3)
    check("monitor: a selection cleared after ordinary text sends nothing",
          h.sent[2:] == ["ordinary"] and not isinstance(h.sent[2], SecretText), [shown(d) for d in h.sent[2:]])
    comp.deliver(("text/plain", PASSWORD.encode()), (HINTS[2], b"secret"))
    await asyncio.sleep(0.3)
    check("monitor: the same secret copied again after its retraction goes out again",
          len(h.sent) == 4 and is_secret(h.sent[3], PASSWORD), [shown(d) for d in h.sent[3:]])
    h.clipboard_running = False
    await asyncio.wait_for(task, 5.0)

    h = handler()
    await h._set_clipboard(SecretText(PASSWORD))
    offered = dict(h.wayland_input.offered)
    check("write-back: a secret is offered with the hint in every spelling",
          offered.get("text/plain") == PASSWORD.encode()
          and all(offered.get(hint) == b"secret" for hint in HINTS), sorted(offered))
    await h._set_clipboard("ordinary")
    check("write-back: ordinary text is offered without it",
          [m for m, _ in h.wayland_input.offered] == ["text/plain"], h.wayland_input.offered)


class Socket:
    closed = False

    def __init__(self) -> None:
        self.frames: list = []

    async def send_str(self, message: str) -> None:
        self.frames.append(message)

    async def send_bytes(self, message: bytes) -> None:
        self.frames.append(message)


class Channel:
    readyState = "open"
    transport = None

    def __init__(self) -> None:
        self.sent: list = []

    def send(self, payload) -> None:
        self.sent.append(json.loads(payload))


async def no_wait(*_args, **_kwargs) -> None:
    return None


async def drained(*_args, **_kwargs) -> bool:
    return True


async def transports() -> None:
    wsm._bulk_pace = no_wait
    wsm._await_bulk_window = no_wait
    wsm.socket_gauge = lambda ws: None
    app = wsm.SelkiesStreamingApp.__new__(wsm.SelkiesStreamingApp)
    sock = Socket()
    app.data_streaming_server = SimpleNamespace(clients={sock}, enable_binary_clipboard=True)
    await app.send_ws_clipboard_data(SecretText(PASSWORD), "text/plain")
    verbs = [f.split(",", 1)[0] for f in sock.frames]
    check("websockets: a secret's payload follows a clipboard_secret frame",
          verbs == ["clipboard_secret", "clipboard"], verbs)
    sock.frames.clear()
    await app.send_ws_clipboard_data(SecretText(""), "text/plain")
    check("websockets: an empty secret goes out, marked", sock.frames == ["clipboard_secret", "clipboard,"],
          sock.frames)
    sock.frames.clear()
    await app.send_ws_clipboard_data(SecretText("x" * 40000), "text/plain")
    check("websockets: a secret too large for one message is marked ahead of its first frame",
          [f.split(",", 1)[0] for f in sock.frames[:2]] == ["clipboard_secret", "clipboard_start"],
          [f[:24] for f in sock.frames[:2]])
    sock.frames.clear()
    await app.send_ws_clipboard_data("ordinary", "text/plain")
    check("websockets: ordinary text carries no mark",
          [f.split(",", 1)[0] for f in sock.frames] == ["clipboard"], sock.frames)
    viewer = Socket()
    app.data_streaming_server.clients = {sock, viewer}
    wsm.client_permissions[viewer] = {"role": "viewer"}
    try:
        sock.frames.clear()
        await app.send_ws_clipboard_data(SecretText(PASSWORD), "text/plain")
        await app.send_ws_clipboard_data("ordinary", "text/plain")
        await app.send_ws_clipboard_data("asked for", "text/plain", reply_to="cr", conn_id=id(viewer))
        verbs = [f.split(",", 1)[0] for f in viewer.frames]
        check("websockets: a viewer is announced nothing, a secret or not, and answered what it asked",
              len(sock.frames) == 3 and verbs == ["clipboard_reply", "clipboard"], verbs)
    finally:
        wsm.client_permissions.pop(viewer, None)

    rte.drain_data_channel = drained
    rtc = rte.RTCApp.__new__(rte.RTCApp)
    channel = Channel()
    rtc.peer_connections = {"peer": {"peer_conn": SimpleNamespace(connectionState="connected"),
                                     "data_channel": channel, "client_type": rte.ClientType.CONTROLLER}}
    await rtc.send_clipboard_data(SecretText(PASSWORD), "text/plain")
    check("webrtc: a secret's payload carries secret",
          channel.sent[-1]["type"] == "clipboard-msg" and channel.sent[-1]["data"].get("secret") is True,
          channel.sent[-1]["data"].keys())
    await rtc.send_clipboard_data(SecretText(""), "text/plain")
    check("webrtc: an empty secret goes out, marked",
          channel.sent[-1]["data"].get("secret") is True and channel.sent[-1]["data"]["content"] == "",
          channel.sent[-1])
    await rtc.send_clipboard_data(SecretText("x" * 70000), "text/plain")
    start = next(m for m in channel.sent if m["type"] == "clipboard-msg-start")
    check("webrtc: a secret too large for one message is marked on its start",
          start["data"].get("secret") is True, start["data"])
    channel.sent.clear()
    await rtc.send_clipboard_data("ordinary", "text/plain")
    await rtc.send_clipboard_data("", "text/plain")
    check("webrtc: ordinary text carries no mark, and an empty one is still not sent",
          len(channel.sent) == 1 and "secret" not in channel.sent[0]["data"], channel.sent)
    viewer = Channel()
    rtc.peer_connections["viewer"] = {"peer_conn": SimpleNamespace(connectionState="connected"),
                                      "data_channel": viewer, "client_type": rte.ClientType.VIEWER}
    channel.sent.clear()
    await rtc.send_clipboard_data(SecretText(PASSWORD), "text/plain")
    await rtc.send_clipboard_data("ordinary", "text/plain")
    await rtc.send_clipboard_data("asked for", "text/plain", reply_to="cr", peer_id="viewer")
    check("webrtc: a viewer is announced nothing, a secret or not, and answered what it asked",
          len(channel.sent) == 2 and [m["data"].get("reply_to") for m in viewer.sent] == ["cr"],
          [m["type"] for m in viewer.sent])


async def main() -> None:
    x11_reads()
    native_reads()
    await app_reads()
    await monitor_loop()
    await transports()


asyncio.run(main())
failed = [label for label, ok in results if not ok]
print(f"[clip-secret] {len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
