#!/usr/bin/env python3
"""Text typed by clipboard paste reaches an app however long it takes to fetch it.

On a compositor with no zwp_virtual_keyboard (KWin) the server types by putting
the text on the app compositor's clipboard, sending Shift+Insert, and putting
back what was copied before. Here the app compositor is a nested labwc and the
focused app is `wl-paste`, started when Insert goes down and after a delay: a
slow app (past the fixed 150 ms margin) and a quick one each paste the typed
text, the clipboard is put back once the fetch is done (soon after a quick
one), the earlier copy is what the clipboard holds afterwards, and with no app
pasting the clipboard is put back after the most the server waits; the
server reading the clipboard itself meanwhile (its monitor does) is no paste.

Usage: python3 tests/integration/test_clipboard_paste_fetch.py
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
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = ["selkies"]

import helpers as H  # noqa: E402
from selkies.input_handler import (  # noqa: E402
    CLIPBOARD_PASTE_FETCH_MAX_S, CLIPBOARD_PASTE_FETCH_S, WebRTCInput)
from test_clipboard_secret import boot_labwc  # noqa: E402

INSERT = 0xFF63
TYPED = "안녕하세요"


def handler(client, socket: str) -> WebRTCInput:
    """A WebRTCInput typing into the nested labwc through pixelflux's client,
    its key events recorded and, on Insert, the app's paste started."""
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = True
    h.wayland_input = client
    h._app_wayland_display = lambda: socket
    h._has_separate_app_compositor = lambda: True
    h._invalidate_app_wl_display_if_dead = lambda display: None
    h._app_clip_read_failure = None
    h._clipboard_last_bytes = None
    h._clipboard_self_write = None
    h._x11_clipboard_monitor = None

    async def no_x11_monitor():
        return None
    h._ensure_x11_clipboard_monitor_async = no_x11_monitor
    h._clipboard_inject_lock = asyncio.Lock()
    h._clipboard_inject_active = False
    h.active_modifiers = set()
    h.paste_delay = h.own_read_delay = None
    h.pasted = []
    h.chord_at = h.fetched_at = h.written_at = None

    def paste(delay: float) -> None:
        time.sleep(delay)
        env = dict(os.environ, WAYLAND_DISPLAY=socket)
        out = subprocess.run(["wl-paste", "--no-newline", "--type", "text/plain;charset=utf-8"],
                             env=env, capture_output=True, timeout=10)
        h.fetched_at = time.monotonic()
        h.pasted.append(out.stdout.decode("utf-8", "replace"))

    def own_read(delay: float) -> None:
        time.sleep(delay)
        client.clipboard_read_app(socket, "text/plain;charset=utf-8")

    async def send(keysym, down=True, neutralize=None):
        if keysym == INSERT and down:
            h.chord_at = time.monotonic()
            if h.paste_delay is not None:
                threading.Thread(target=paste, args=(h.paste_delay,), daemon=True).start()
            if h.own_read_delay is not None:
                threading.Thread(target=own_read, args=(h.own_read_delay,), daemon=True).start()
    h.send_x11_keypress = send

    real_write = h.write_clipboard

    async def write(data, mime_type="text/plain", flavours=None):
        h.written_at = time.monotonic()
        return await real_write(data, mime_type, flavours)
    h.write_clipboard = write
    return h


async def type_once(h: WebRTCInput, client, socket: str, delay, own_read=None) -> dict:
    """Copy "before", type TYPED with the app pasting `delay` s after the
    chord (None: no app pastes) and the server reading the clipboard itself
    `own_read` s after it, and report what happened."""
    client.clipboard_write_app(socket, [("text/plain;charset=utf-8", b"before"),
                                        ("text/plain", b"before")])
    h.paste_delay, h.own_read_delay, h.pasted, h.fetched_at = delay, own_read, [], None
    ok = await h._inject_text_via_clipboard(TYPED)
    for _ in range(100):
        if delay is None or h.pasted:
            break
        await asyncio.sleep(0.05)
    after = client.clipboard_read_app(socket, "text/plain;charset=utf-8")
    return {"ok": ok, "pasted": h.pasted[:1], "restored_after": h.written_at - h.chord_at,
            "fetched_after": None if h.fetched_at is None else h.fetched_at - h.chord_at,
            "clipboard_after": after.decode() if after else after}


def main() -> H.Results:
    res = H.Results("clip-paste-fetch")
    for tool in ("labwc", "wl-paste"):
        if shutil.which(tool) is None:
            res.skip("paste fetch on a nested labwc", f"{tool} is not installed")
            res.summary()
            return res
    import pixelflux
    runtime = tempfile.mkdtemp(prefix="pastefetch-")
    os.chmod(runtime, 0o700)
    saved = os.environ.get("XDG_RUNTIME_DIR")
    os.environ["XDG_RUNTIME_DIR"] = runtime
    proc, socket = boot_labwc(runtime)
    try:
        if not socket:
            res.check("labwc came up", False, H.tail(os.path.join(runtime, "labwc.log")))
            return res
        client = pixelflux.ScreenCapture()
        h = handler(client, socket)
        ms = lambda s: "-" if s is None else f"{s * 1000:.0f} ms"  # noqa: E731

        slow = asyncio.run(type_once(h, client, socket, 0.3))
        res.check("an app fetching past the fixed margin pastes the typed text",
                  slow["ok"] and slow["pasted"] == [TYPED], slow["pasted"])
        res.check("the clipboard is put back only once that fetch is done",
                  slow["fetched_after"] is not None
                  and slow["restored_after"] >= slow["fetched_after"] - 0.02,
                  f"fetched {ms(slow['fetched_after'])}, put back {ms(slow['restored_after'])}")
        res.check("the clipboard holds the earlier copy afterwards",
                  slow["clipboard_after"] == "before", slow["clipboard_after"])

        quick = asyncio.run(type_once(h, client, socket, 0.0))
        res.check("a quick app pastes the typed text", quick["pasted"] == [TYPED], quick["pasted"])
        res.check("and the clipboard is put back sooner than the fixed margin",
                  quick["restored_after"] < CLIPBOARD_PASTE_FETCH_S,
                  f"fetched {ms(quick['fetched_after'])}, put back {ms(quick['restored_after'])}")

        own = asyncio.run(type_once(h, client, socket, 0.3, own_read=0.05))
        res.check("the server's own read of the clipboard meanwhile is not the paste",
                  own["pasted"] == [TYPED] and own["restored_after"] >= 0.25,
                  f"{own['pasted']}, fetched {ms(own['fetched_after'])}, put back {ms(own['restored_after'])}")

        none = asyncio.run(type_once(h, client, socket, None))
        res.check("with no app pasting, the clipboard is put back after the most the server waits",
                  CLIPBOARD_PASTE_FETCH_MAX_S - 0.02 <= none["restored_after"]
                  < CLIPBOARD_PASTE_FETCH_MAX_S + 0.3 and none["clipboard_after"] == "before",
                  f"put back {ms(none['restored_after'])}, {none['clipboard_after']!r}")
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
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
