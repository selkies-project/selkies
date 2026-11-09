#!/usr/bin/env python3
"""A page opened at a URL carrying `user:pass@` (a link that logs in) drops them
before its scripts run. Left in `document.URL`, they make every relative
`fetch()` throw (the dashboard's `/api/status` probe, the mode switch, the
manifest), and Chrome and Safari run a module that a chunk imports back a second
time. The browser keeps the login it took from the URL, so the page reloads at
the same URL without them and streams as it would have.

Each engine opens the dashboard that way under Basic auth: it ends at the URL
without the credentials, its `/api/status` probe published the mode, it streams
with one core connected, and a relative fetch answers. Chromium does the same on
the core's own page. WebKit attaches no Basic credentials to a WebSocket
handshake, which presents the socket cookie the page load was answered with
instead; it also streams at a plain URL with the login given to the browser.
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C

from playwright.sync_api import sync_playwright

USER, PASSWORD = "selkies", "hunter2"
passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [credentials-in-url] {label}  {detail}", flush=True)


def settled_url(page, seconds: float = 15) -> str:
    """The document's URL once a page that reloads itself has stopped navigating."""
    deadline = time.time() + seconds
    url = ""
    while time.time() < deadline:
        try:
            url = page.evaluate("document.readyState === 'complete' ? document.URL : ''")
            if url and "@" not in url:
                return url
        except Exception:
            pass
        time.sleep(0.25)
    return url


def visit(p, engine: str, page_name: str, in_url: bool = True) -> None:
    browser = C.launch_browser(p, engine)
    try:
        page = browser.new_page(viewport={"width": 1280, "height": 800}, **(
            {} if in_url else {"http_credentials": {"username": USER, "password": PASSWORD}}))
        log_from = os.path.getsize(H.LOG) if os.path.exists(H.LOG) else 0
        page.goto((H.BASE_URL.replace("://", f"://{USER}:{PASSWORD}@") if in_url else H.BASE_URL) + "/",
                  wait_until="domcontentloaded")
        tag = f"{engine}, {page_name}" + ("" if in_url else ", the login given to the browser")
        url = settled_url(page)
        check(f"{tag}: the page ends at the URL without the credentials", url and "@" not in url, url)
        info = C.wait_ws_video(page, timeout=30)
        check(f"{tag}: the stream comes up", info is not None, info)
        if page_name == "dashboard":
            mode = page.evaluate("window.__SELKIES_STREAMING_MODE__")
            check(f"{tag}: the /api/status probe published the mode", mode == "websockets", mode)
        page.wait_for_timeout(2000)
        with open(H.LOG, errors="replace") as f:
            f.seek(log_from)
            sockets = len(re.findall(r"Client \S+ connected \(", f.read()))
        check(f"{tag}: one core connected", sockets == 1, f"{sockets} stream sockets")
        status = page.evaluate("fetch('manifest.json').then(r => r.status, e => String(e))")
        check(f"{tag}: a relative fetch answers", status in (200, 404), status)
    finally:
        browser.close()


def main() -> None:
    env = {"SELKIES_ENABLE_BASIC_AUTH": "true", "SELKIES_BASIC_AUTH_USER": USER,
           "SELKIES_BASIC_AUTH_PASSWORD": PASSWORD}
    try:
        H.server_start(mode="websockets", wayland=False, web_root=H.DASHBOARD_DIST, extra_env=env)
        with sync_playwright() as p:
            for engine in ("chromium", "firefox", "webkit"):
                visit(p, engine, "dashboard")
            visit(p, "webkit", "dashboard", in_url=False)
        H.server_stop()
        H.server_start(mode="websockets", wayland=False, extra_env=env)
        with sync_playwright() as p:
            visit(p, "chromium", "core page")
    finally:
        H.server_stop()
    print(f"[credentials-in-url] {passed}/{passed + failed} passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
