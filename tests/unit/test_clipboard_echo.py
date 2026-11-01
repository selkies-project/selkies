#!/usr/bin/env python3
"""A client's push of the session clipboard's own text leaves it to its owner.

The browser hands the session's copy back in its own form: Chromium on Windows
keeps plain text with CRLF line ends, and markup comes back rewrapped. Written,
that would take the clipboard from its owner with less than it offered, and a
file manager's cut (a URI list marked as a cut, beside its paths as text) would
become the text. Any other push is written as before.
"""
import asyncio
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.input_handler import CLIPBOARD_FLAVOURS_MIME, SecretText, WebRTCInput, clipboard_envelope  # noqa: E402

results = []


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [clip-echo] {label}  {detail}", flush=True)


def handler(baseline: bytes, mime: str) -> WebRTCInput:
    """A WebRTCInput whose session clipboard last read `baseline` (`mime`), recording each write."""
    h = WebRTCInput.__new__(WebRTCInput)
    h._bg_tasks = set()
    h._x11_clipboard_monitor = None
    h._clipboard_last_bytes = baseline
    h._clipboard_last_mime = mime
    h._clipboard_self_write = None
    h._png_beside = None
    h.pressed_keys = {}
    h.writes = []

    async def set_clipboard(data, mime_type="text/plain", flavours=None):
        h.writes.append((mime_type, data))
        return True
    h._set_clipboard = set_clipboard
    return h


async def main() -> None:
    paths = b"file:///home/u/report.odt\nfile:///home/u/notes.txt"
    h = handler(paths, "text/plain")
    ok = await h.write_clipboard(paths.replace(b"\n", b"\r\n").decode())
    check("the session's own text with CRLF line ends is no copy", ok and h.writes == [], h.writes)
    ok = await h.write_clipboard(paths.decode() + "\n")
    check("nor with a line end after it", ok and h.writes == [], h.writes)
    await h.write_clipboard("file:///home/u/other.txt")
    check("other text is written", h.writes == [("text/plain", "file:///home/u/other.txt")], h.writes)

    h = handler(b"some text", "text/plain")
    await h.write_clipboard(SecretText("some text"))
    check("a secret is written even when its text is the session's, for its hint", len(h.writes) == 1, h.writes)

    markup = clipboard_envelope([("text/html", b"<p>from <b>the session</b></p>"), ("text/plain", b"from the session")])
    h = handler(markup, CLIPBOARD_FLAVOURS_MIME)
    back = [("text/html", b"<meta charset='utf-8'><p>from <b>the session</b></p>"), ("text/plain", b"from the session")]
    await h.write_clipboard(back[0][1], "text/html", back)
    check("the session's formatted copy come back rewrapped is no copy", h.writes == [], h.writes)
    await h.write_clipboard("from the session")
    check("nor is its plain text alone", h.writes == [], h.writes)
    edited = [("text/html", b"<p>edited</p>"), ("text/plain", b"edited")]
    await h.write_clipboard(edited[0][1], "text/html", edited)
    check("a different formatted copy is written", len(h.writes) == 1, h.writes)

    h = handler(b"\x89PNG...", "image/png")
    await h.write_clipboard("text")
    check("over an image, text is written", len(h.writes) == 1, h.writes)
    h = handler(None, None)
    await h.write_clipboard("text")
    check("with nothing read yet, text is written", len(h.writes) == 1, h.writes)


asyncio.run(main())
failed = [label for label, ok in results if not ok]
print(f"[clip-echo] {len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
