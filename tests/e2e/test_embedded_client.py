#!/usr/bin/env python3
"""The web client the server carries: served from memory where no web_root holds one.

Every other suite points `web_root` at a client built in the tree. Here none is set, so the
server answers from the table compiled into it, and says so at start. The entry page and a
content-hashed script it loads come back gzip-encoded with their entity tags and dates, a
revalidation is a 304, and a client naming no encoding gets the same bytes inflated. A
browser loading the page streams over the transport the block names (websockets, webrtc),
and a second load takes the hashed assets from its cache without a request, while the entry
page and the core it loads under a fixed name are revalidated.
"""
import gzip
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright


def fetch(path: str, headers: dict) -> tuple:
    """`(status, headers, body)` of a GET of `path`, a 304 included."""
    request = urllib.request.Request(f"{H.BASE_URL}{path}", headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, response.headers, response.read()
    except urllib.error.HTTPError as err:
        return err.code, err.headers, err.read()


def http_block(res: H.Results) -> None:
    """The entry page and a hashed script by encoding and validator."""
    status, headers, body = fetch("/", {"Accept-Encoding": "gzip"})
    page = gzip.decompress(body) if headers.get("Content-Encoding") == "gzip" else body
    res.check("the entry page is served", status == 200 and b"selkies-core.js" in page, status)
    res.check("the entry page is revalidated on every load", headers.get("Cache-Control") == "no-cache",
              headers.get("Cache-Control"))
    found = re.search(rb'src="\./(assets/[^"]+\.js)"', page)
    res.check("the entry page loads a content-hashed script", found is not None, page[:120])
    if found is None:
        return
    asset = "/" + found.group(1).decode()
    status, headers, body = fetch(asset, {"Accept-Encoding": "gzip, deflate, br"})
    tag = headers.get("ETag", "")
    res.check(f"{asset} goes out gzip-encoded to a client that takes it",
              status == 200 and headers.get("Content-Encoding") == "gzip"
              and headers.get("Vary", "").lower() == "accept-encoding",
              (status, headers.get("Content-Encoding"), headers.get("Vary")))
    res.check(f"{asset} carries a strong tag and a date and no Cache-Control",
              tag.startswith('"') and headers.get("Last-Modified") and headers.get("Cache-Control") is None,
              (tag, headers.get("Last-Modified"), headers.get("Cache-Control")))
    status, headers, plain = fetch(asset, {})
    res.check(f"{asset} goes out as it is to a client naming no encoding",
              status == 200 and headers.get("Content-Encoding") is None and plain == gzip.decompress(body),
              (status, headers.get("Content-Encoding"), len(plain)))
    status, _, empty = fetch(asset, {"Accept-Encoding": "gzip", "If-None-Match": tag})
    res.check(f"{asset} revalidates with a 304", status == 304 and not empty, status)


def browser_block(res: H.Results, mode: str) -> None:
    """The page streams, and a second load reaches the server only for what revalidates."""
    with sync_playwright() as pw:
        browser = C.chromium_launch(pw)
        try:
            context = browser.new_context(viewport={"width": 1280, "height": 720})
            context.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            page = context.new_page()
            try:
                page.goto(f"{H.BASE_URL}/", wait_until="load")
            except Exception as err:
                res.check(f"the page streams over {mode}", False, err)
                return
            video = C.wait_ws_video(page, 30) if mode == "websockets" else C.wait_wr_video(page, 45)
            res.check(f"the page streams over {mode}", video is not None, video)
            page.close()
            page = context.new_page()
            page.goto(f"{H.BASE_URL}/", wait_until="load")
            loads = page.evaluate("""(() => performance.getEntriesByType('resource')
              .concat(performance.getEntriesByType('navigation'))
              .map((e) => ({name: new URL(e.name).pathname, transfer: e.transferSize})))()""")
            assets = [e for e in loads if e["name"].startswith("/assets/")]
            fetched = [e["name"] for e in assets if e["transfer"] > 0]
            res.check("a second load takes every hashed asset from the browser's cache",
                      assets and not fetched, fetched[:3] or len(assets))
            asked = {e["name"]: e["transfer"] for e in loads if e["name"] in ("/", "/selkies-core.js")}
            res.check("and asks the server again for the entry page and the core",
                      len(asked) == 2 and all(size > 0 for size in asked.values()), asked)
        finally:
            browser.close()


def run_block(mode: str) -> H.Results:
    res = H.Results(f"embedded-client-{mode}")
    version = subprocess.run([H.SERVER, "--version"], capture_output=True, text=True).stdout
    res.check("the server under test embeds a web client", "web client: embedded (" in version,
              version.strip().splitlines()[-1:])
    H.server_start(mode=mode, web_root="")
    try:
        res.check("the server says it serves the client it embeds",
                  C.wait_log("Serving the web client from this executable"),
                  [line for line in H.server_log().splitlines() if "web client" in line][:1])
        http_block(res)
        browser_block(res, mode)
    finally:
        H.server_stop()
    res.summary()
    return res


def main() -> None:
    """Run the blocks named on argv: websockets, webrtc, or both by default."""
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = [run_block(mode) for mode in ("websockets", "webrtc") if which in ("all", mode)]
    failed = sum(len(block.failed()) for block in blocks)
    sys.exit(1 if failed or not blocks else 0)


if __name__ == "__main__":
    main()
