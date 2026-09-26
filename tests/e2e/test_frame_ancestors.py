#!/usr/bin/env python3
"""Which pages may show the client in a frame is the operator's to name.

A platform that embeds the desktop in its own UI needs the client framed, so an
unset `frame_ancestors` leaves framing open, as it always was. Set, it is sent
as the Content-Security-Policy frame-ancestors directive on every response, and
the browser refuses a frame the list does not name: `'self'` shuts out a page
on any other origin, and a listed origin gets its frame.

A page on a second origin frames the client under each policy, and Chromium's
own verdict is read from what the frame ends up showing.
"""
import http.server
import os
import sys
import threading
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

res = H.Results("frame-ancestors")


class Embedder(http.server.BaseHTTPRequestHandler):
    """A page on another origin (localhost rather than the client's 127.0.0.1) that frames the client."""

    def do_GET(self) -> None:
        body = f'<html><body><iframe id="f" src="{H.BASE_URL}/"></iframe></body></html>'.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args) -> None:
        pass


def framed(browser, embed_url: str) -> bool:
    """Whether the client came up inside the other origin's frame."""
    page = browser.new_page()
    try:
        page.goto(embed_url, wait_until="load")
        deadline = time.time() + 10
        while time.time() < deadline:
            child = next((f for f in page.frames if f is not page.main_frame), None)
            if child is not None and child.url.startswith(f"{H.BASE_URL}/"):
                try:
                    if child.evaluate("document.readyState") == "complete":
                        return True
                except Exception:
                    pass
            time.sleep(0.25)
        return False
    finally:
        page.close()


def policy() -> str:
    with urllib.request.urlopen(f"{H.BASE_URL}/", timeout=5) as response:
        return response.headers.get("Content-Security-Policy", "")


def main() -> int:
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Embedder)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    embed_origin = f"http://localhost:{server.server_address[1]}"
    cases = (
        ("unset", None, "", True),
        ("'self'", "'self'", "frame-ancestors 'self'", False),
        ("the embedding origin", f"self, {embed_origin}", f"frame-ancestors 'self' {embed_origin}", True),
    )
    try:
        with sync_playwright() as pw:
            browser = C.chromium_launch(pw)
            for label, value, header, allowed in cases:
                H.server_start(extra_env={"SELKIES_FRAME_ANCESTORS": value} if value is not None else None)
                try:
                    sent = policy()
                    res.check(f"{label}: the server sends {header or 'no frame policy'}", sent == header, sent)
                    shown = framed(browser, f"{embed_origin}/")
                    res.check(f"{label}: another origin's frame {'shows' if allowed else 'is refused'} the client",
                              shown == allowed, f"framed={shown}")
                finally:
                    H.server_stop()
            browser.close()
    finally:
        server.shutdown()
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
