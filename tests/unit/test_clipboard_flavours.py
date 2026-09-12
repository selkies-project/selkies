#!/usr/bin/env python3
"""A copy keeps its flavours across the wire.

Markup and the plain text its source wrote for it travel together in one
envelope, so pasting into a rich editor takes the markup and pasting into a
plain field takes the text rather than a rendering of it. The envelope is
text, so the image clipboard's switch is not its switch, and a direction the
operator closed still closes it.
"""
import asyncio
import base64
import json
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.argv = ["selkies"]

from selkies.input_handler import (  # noqa: E402
    CLIPBOARD_FLAVOURS_MIME, WebRTCInput, clipboard_envelope, clipboard_flavours)
from selkies.settings import SETTING_DEFINITIONS  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [clip-flavours] {label}  {str(detail)[:120]}", flush=True)


def envelope_cases() -> None:
    entries = [("text/html", "<b>héllo</b>".encode()), ("text/plain", "héllo".encode())]
    packed = clipboard_envelope(entries)
    check("an envelope round trips both flavours", clipboard_flavours(packed) == entries,
          clipboard_flavours(packed))
    check("it is JSON a collector or a browser can read without base64",
          json.loads(packed.decode())["text/plain"] == "héllo", packed[:60])
    check("markup alone is a valid envelope",
          clipboard_flavours(clipboard_envelope([("text/html", b"<i>x</i>")]))
          == [("text/html", b"<i>x</i>")])
    check("the richest flavour comes first whatever the order in",
          clipboard_flavours(clipboard_envelope(list(reversed(entries))))[0][0] == "text/html")
    for junk, why in ((b"[]", "a list"), (b"{}", "an empty object"),
                      (b'{"image/png": "x"}', "no text flavour"),
                      (b'{"text/html": ""}', "an empty flavour")):
        try:
            clipboard_flavours(junk)
            check(f"{why} is refused", False, "accepted")
        except ValueError:
            check(f"{why} is refused", True)


class _Handler(WebRTCInput):
    """The dispatcher with the clipboard write stubbed out."""

    def __init__(self, **policy) -> None:
        self.enable_clipboard = policy.get("enable_clipboard", "true")
        self.enable_binary_clipboard = policy.get("enable_binary_clipboard", "true")
        self._reset_multipart_clipboard()
        self.written: list = []

    async def write_clipboard(self, data, mime_type="text/plain", flavours=None):
        self.written.append((mime_type, data, flavours))
        return True


def wire(entries) -> str:
    payload = base64.b64encode(clipboard_envelope(entries)).decode()
    return f"cb,{CLIPBOARD_FLAVOURS_MIME},{payload}"


async def dispatch_cases() -> None:
    entries = [("text/html", b"<b>rich</b>"), ("text/plain", b"rich")]

    handler = _Handler()
    await handler._dispatch_message(wire(entries))
    check("a flavour set is written as the markup, with every flavour offered",
          handler.written == [("text/html", b"<b>rich</b>", entries)], handler.written)

    handler = _Handler(enable_binary_clipboard="false")
    await handler._dispatch_message(wire(entries))
    check("the image switch does not gate it", len(handler.written) == 1, handler.written)
    await handler._dispatch_message("cb,image/png," + base64.b64encode(b"\x89PNG").decode())
    check("an image still obeys that switch", len(handler.written) == 1, handler.written)

    handler = _Handler(enable_clipboard="out")
    await handler._dispatch_message(wire(entries))
    check("a closed inbound direction still closes it", handler.written == [], handler.written)

    handler = _Handler()
    await handler._dispatch_message(f"cb,{CLIPBOARD_FLAVOURS_MIME},{base64.b64encode(b'{}').decode()}")
    check("a malformed envelope writes nothing and raises nothing", handler.written == [])

    handler = _Handler()
    raw = clipboard_envelope(entries)
    half = len(raw) // 2
    await handler._dispatch_message(f"cbs,t1,{CLIPBOARD_FLAVOURS_MIME},{len(raw)}")
    for chunk in (raw[:half], raw[half:]):
        await handler._dispatch_message(f"cbd,t1,{base64.b64encode(chunk).decode()}")
    await handler._dispatch_message("cbe,t1")
    check("a multipart flavour set arrives whole",
          handler.written == [("text/html", b"<b>rich</b>", entries)], handler.written)


def settings_cases() -> None:
    names = {d["name"]: d for d in SETTING_DEFINITIONS}
    for name in ("clipboard_seamless", "keyboard_shortcuts"):
        check(f"{name} is a client-overridable bool defaulting on",
              names.get(name, {}).get("type") == "bool" and names[name]["default"] is True,
              names.get(name))


def main() -> int:
    envelope_cases()
    asyncio.run(dispatch_cases())
    settings_cases()
    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
