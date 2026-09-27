#!/usr/bin/env python3
"""A client's image in another format reaches the session as PNG as well.

Most applications paste no image type but PNG, while an image uploaded from
disk arrives as the JPEG or WebP it was saved as, so the server offers a PNG of
it beside it, first. The client's later messages, its input among them, wait
for the write, so the conversion is waited for only briefly: a photo taking
longer is offered as it came at once and the PNG joins it when ready, unless a
newer copy took the clipboard meanwhile. The offers are recorded, not made.
"""
import asyncio
import io
import os
import sys
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from PIL import Image  # noqa: E402

from selkies import input_handler as ih  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402

results = []


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [clip-png] {label}  {detail}", flush=True)


def jpeg(width: int, height: int) -> bytes:
    buf = io.BytesIO()
    Image.new("RGB", (width, height), (200, 40, 40)).save(buf, "JPEG")
    return buf.getvalue()


def make_handler() -> WebRTCInput:
    """A WebRTCInput whose native offer is recorded: each `(entries, when)`."""
    h = WebRTCInput.__new__(WebRTCInput)
    h._bg_tasks = set()
    h._x11_clipboard_monitor = None
    h._clipboard_last_bytes = None
    h._clipboard_self_write = None
    h.offers = []

    async def set_clipboard(data, mime_type="text/plain", flavours=None):
        payload = data if isinstance(data, bytes) else data.encode()
        h._clipboard_last_bytes = payload
        h.offers.append(([m for m, _ in (flavours or [(mime_type, payload)])], time.monotonic()))
        return True
    h._set_clipboard = set_clipboard
    return h


async def main() -> None:
    photo = jpeg(40, 30)
    converted = ih.clipboard_png_beside("image/jpeg", photo)
    check("a JPEG converts to a PNG of the same pixels",
          converted is not None and Image.open(io.BytesIO(converted)).size == (40, 30))
    check("a PNG, text, or undecodable bytes get none",
          ih.clipboard_png_beside("image/png", converted) is None
          and ih.clipboard_png_beside("text/plain", b"x") is None
          and ih.clipboard_png_beside("image/webp", b"not an image") is None)

    # Quick to convert: one offer, PNG first, and PNG is what reads back.
    h = make_handler()
    await h.write_clipboard(photo, "image/jpeg")
    check("a quick conversion is offered once, PNG first",
          [o[0] for o in h.offers] == [["image/png", "image/jpeg"]], h.offers)

    real = ih.clipboard_png_beside

    def slow(mime_type, data):
        time.sleep(0.8)
        return real(mime_type, data)

    ih.clipboard_png_beside = slow
    try:
        # Slow: the write returns within the bound with the image as it came.
        h = make_handler()
        start = time.monotonic()
        await h.write_clipboard(photo, "image/jpeg")
        took = time.monotonic() - start
        check("a slow conversion keeps the client's messages waiting no longer than the bound",
              took < ih.CLIPBOARD_PNG_WAIT_S + 0.2 and [o[0] for o in h.offers] == [["image/jpeg"]],
              f"{took:.2f}s {h.offers}")
        await asyncio.sleep(1.2)
        check("its PNG joins it once ready",
              [o[0] for o in h.offers] == [["image/jpeg"], ["image/png", "image/jpeg"]], h.offers)

        # A newer copy lands while the PNG is still being made: it stays.
        h = make_handler()
        await h.write_clipboard(photo, "image/jpeg")
        await h._set_clipboard("copied since", "text/plain")
        await asyncio.sleep(1.2)
        check("a copy made while the PNG was being made keeps the clipboard",
              [o[0] for o in h.offers] == [["image/jpeg"], ["text/plain"]], h.offers)
    finally:
        ih.clipboard_png_beside = real


asyncio.run(main())
failed = [label for label, ok in results if not ok]
print(f"[clip-png] {len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
