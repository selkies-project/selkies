#!/usr/bin/env python3
"""Secure mode binds the API routes to the session token, on a real server.

routes:      with a master token set and Basic auth off, /api/upload,
             /api/files/ (listing and download), /api/turn and /api/metrics
             refuse a request without a token and take one as a Bearer
             header, as ?token=, or as the client's cookie; a viewer-role
             token cannot upload; the master token is accepted everywhere;
             liveness and the static client stay open; the data WebSocket
             still takes its token in the handshake.
websockets:  a browser page loaded with ?token= streams over WebSockets,
             sets the API cookie, uploads through the file input with a
             Bearer header, and opens the file listing (whose links keep the
             token); a viewer page's upload is refused.
webrtc:      the same page over WebRTC fetches its TURN configuration with
             the Bearer header and streams.
dashboards:  both dashboards open their file manager with the page's token
             and the listing renders inside the modal; the classic one
             switches transport on that same token, asking the user for
             nothing (the master token is the operator's, not a session
             user's, so a prompt for it would be unanswerable).
legacy:      without a master token nothing changes: the routes are open with
             Basic auth off and Basic-gated, view-only password included,
             with it on.
"""
import asyncio
import base64
import http.client
import json
import os
import shutil
import sys
import tempfile
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
import websockets

MASTER = "e2e-master-token"
CTRL_TOKEN = "e2e-ctrl-Qx9"
VIEW_TOKEN = "e2e-view-Zt4"
COOKIE = "selkies_token"
BEARER_CHALLENGE = 'Bearer realm="Selkies Restricted"'

SCRATCH = tempfile.mkdtemp(prefix="selkies-secure-mode-")
FILES_DIR = os.path.join(SCRATCH, "files")


def request(method: str, path: str, headers=None, body=None) -> tuple:
    """One request against the test server.

    Returns:
        `(status, response headers, body bytes)`; 4xx/5xx are not raised.
    """
    conn = http.client.HTTPConnection("localhost", H.PORT, timeout=15)
    try:
        conn.request(method, path, body=body, headers=dict(headers or {}))
        response = conn.getresponse()
        return response.status, {k.lower(): v for k, v in response.getheaders()}, response.read()
    finally:
        conn.close()


def bearer(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def cookie(token: str) -> dict:
    return {"Cookie": f"{COOKIE}={urllib.parse.quote(token, safe='')}"}


def upload(name: str, data: bytes, headers=None) -> int:
    status, _, _ = request("POST", "/api/upload", headers=dict(headers or {}, **{
        "X-Upload-Path": urllib.parse.quote(name), "Content-Type": "application/octet-stream"}), body=data)
    return status


def post_tokens(table: dict) -> int:
    status, _, _ = request("POST", "/api/tokens", headers=dict(bearer(MASTER), **{"Content-Type": "application/json"}),
                        body=json.dumps(table).encode())
    return status


def fresh_files_dir() -> None:
    """An empty file-manager directory holding one downloadable file."""
    shutil.rmtree(FILES_DIR, ignore_errors=True)
    os.makedirs(os.path.join(FILES_DIR, "sub"))
    with open(os.path.join(FILES_DIR, "hello.txt"), "wb") as f:
        f.write(b"hello from the file manager\n")


def secure_env(**extra) -> dict:
    env = {"SELKIES_MASTER_TOKEN": MASTER, "SELKIES_FILE_MANAGER_PATH": FILES_DIR,
           "SELKIES_ENABLE_METRICS_HTTP": "true"}
    env.update(extra)
    return env


def wait_file(path: str, timeout: float = 15) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if os.path.exists(path):
            return True
        time.sleep(0.25)
    return False


async def ws_handshake(query: str, seconds: float = 3.0) -> tuple:
    """Connect the data socket and collect the handshake's text messages."""
    uri = f"ws://localhost:{H.PORT}/api/websockets{query}"
    messages = []
    close_code = None
    try:
        async with websockets.connect(uri, max_size=None) as ws:
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=0.5)
                except asyncio.TimeoutError:
                    continue
                if isinstance(msg, str):
                    messages.append(msg)
                    if msg.startswith("MK_ACCESS"):
                        break
    except websockets.exceptions.ConnectionClosed as e:
        close_code = e.rcvd.code if e.rcvd else e.code
    except Exception as e:
        messages.append(f"ERROR {e!r}")
    return messages, close_code


def provision(res: "H.Results") -> None:
    status = post_tokens({
        CTRL_TOKEN: {"role": "controller", "slot": 1},
        VIEW_TOKEN: {"role": "viewer", "slot": None},
    })
    res.check("tokens provisioned", status == 200, status)


def run_routes() -> "H.Results":
    res = H.Results("routes")
    fresh_files_dir()
    H.server_start(mode="websockets", wayland=False, extra_env=secure_env())
    provision(res)

    for method, path in (("POST", "/api/upload"), ("GET", "/api/files/"), ("GET", "/api/files/hello.txt"),
                         ("GET", "/api/turn"), ("GET", "/api/metrics")):
        status, headers, _ = request(method, path, headers={"X-Upload-Path": "nope.txt"}, body=b"x")
        res.check(f"no token: {method} {path} refused with the Bearer challenge",
                  status == 401 and headers.get("www-authenticate") == BEARER_CHALLENGE,
                  f"{status} {headers.get('www-authenticate')!r}")
    for path in ("/api/status", "/api/health", "/"):
        status, _, body = request("GET", path)
        res.check(f"no token: GET {path} stays open", status == 200, status)
    status, _, body = request("GET", "/")
    res.check("the static client is served without a token", b"<html" in body.lower() or b"<!doctype" in body.lower(), body[:60])

    res.check("Bearer controller: upload accepted", upload("up-bearer.txt", b"bearer", bearer(CTRL_TOKEN)) == 200)
    res.check("Bearer controller: the upload landed",
              wait_file(os.path.join(FILES_DIR, "up-bearer.txt"), 5)
              and open(os.path.join(FILES_DIR, "up-bearer.txt"), "rb").read() == b"bearer")
    res.check("cookie controller: upload accepted (no Origin)", upload("up-cookie.txt", b"cookie", cookie(CTRL_TOKEN)) == 200)
    res.check("cookie controller: the upload landed", wait_file(os.path.join(FILES_DIR, "up-cookie.txt"), 5))
    status, _, _ = request("POST", "/api/upload", headers=dict(cookie(CTRL_TOKEN), Origin="https://evil.example",
                                                             **{"X-Upload-Path": "csrf.txt"}), body=b"x")
    res.check("cookie controller: a cross-site POST is refused by Origin",
              status == 403 and not os.path.exists(os.path.join(FILES_DIR, "csrf.txt")), status)
    status, _, _ = request("POST", f"/api/upload?token={CTRL_TOKEN}", headers={"X-Upload-Path": "up-query.txt"}, body=b"q")
    res.check("?token= controller: upload accepted", status == 200, status)
    res.check("Bearer master: upload accepted", upload("up-master.txt", b"m", bearer(MASTER)) == 200)
    status = upload("up-viewer.txt", b"v", bearer(VIEW_TOKEN))
    res.check("Bearer viewer: upload refused", status == 403 and not os.path.exists(os.path.join(FILES_DIR, "up-viewer.txt")), status)
    status = upload("up-viewer-cookie.txt", b"v", cookie(VIEW_TOKEN))
    res.check("cookie viewer: upload refused", status == 403, status)
    res.check("Bearer prefix of a token: upload refused", upload("up-prefix.txt", b"p", bearer(CTRL_TOKEN[:-1])) == 401)

    for label, headers, path in (("Bearer controller", bearer(CTRL_TOKEN), "/api/files/"),
                                 ("cookie controller", cookie(CTRL_TOKEN), "/api/files/"),
                                 ("?token= controller", {}, f"/api/files/?token={CTRL_TOKEN}"),
                                 ("Bearer viewer", bearer(VIEW_TOKEN), "/api/files/"),
                                 ("Bearer master", bearer(MASTER), "/api/files/")):
        status, _, body = request("GET", path, headers=headers)
        res.check(f"{label}: the file listing is served", status == 200 and b"hello.txt" in body, f"{status} {body[:40]}")
    status, hdrs, body = request("GET", "/api/files/hello.txt", headers=cookie(CTRL_TOKEN))
    res.check("cookie controller: a download is served as an attachment",
              status == 200 and body == b"hello from the file manager\n"
              and "attachment" in hdrs.get("content-disposition", ""), f"{status} {hdrs.get('content-disposition')}")
    status, hdrs, body = request("GET", f"/api/files/hello.txt?token={VIEW_TOKEN}")
    res.check("?token= viewer: a download is served", status == 200 and body.startswith(b"hello"), status)
    status, hdrs, _ = request("GET", "/api/files/sub", headers=bearer(CTRL_TOKEN))
    res.check("a directory without its slash redirects (token carried by the header)",
              status in (301, 308) and hdrs.get("location", "").endswith("/api/files/sub/"), f"{status} {hdrs.get('location')}")
    status, hdrs, _ = request("GET", f"/api/files/sub?token={CTRL_TOKEN}")
    res.check("the redirect keeps the ?token= query", status in (301, 308) and f"token={CTRL_TOKEN}" in hdrs.get("location", ""),
              f"{status} {hdrs.get('location')}")

    for label, headers in (("Bearer controller", bearer(CTRL_TOKEN)), ("cookie viewer", cookie(VIEW_TOKEN)),
                           ("Bearer master", bearer(MASTER))):
        status, _, body = request("GET", "/api/metrics", headers=headers)
        res.check(f"{label}: metrics served", status == 200 and b"# " in body, f"{status} {body[:40]}")
    status, _, _ = request("GET", "/api/turn", headers=bearer(CTRL_TOKEN))
    res.check("Bearer controller: /api/turn passes the gate (409: WebRTC inactive)", status == 409, status)

    msgs, code = asyncio.run(ws_handshake(f"?token={CTRL_TOKEN}"))
    res.check("the data WebSocket still authenticates with ?token=",
              any(m.startswith("AUTH_SUCCESS") for m in msgs), f"{code} {msgs[:3]}")
    msgs, code = asyncio.run(ws_handshake(f"?token={CTRL_TOKEN[:-1]}", seconds=2.0))
    res.check("the data WebSocket still refuses a wrong token", code == 4001, f"{code} {msgs[:2]}")
    log = H.server_log()
    res.check("session tokens never reach the server log",
              CTRL_TOKEN not in log and VIEW_TOKEN not in log, "")
    res.summary()
    return res


def launch(pw, query: str, mode: str, url_hash: str = "") -> tuple:
    """A page on the server with the given query; returns (browser, page, requests)."""
    browser = C.launch_browser(pw, "chromium")
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    page = ctx.new_page()
    requests = []
    page.on("request", lambda r: requests.append((r.method, r.url, dict(r.headers))))
    page.add_init_script("""
      window.__wsFrames = 0;
      (() => {
        const WS = window.WebSocket;
        window.WebSocket = function(...a) {
          const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
          s.addEventListener('message', (e) => { if (e.data instanceof ArrayBuffer) window.__wsFrames++; });
          return s;
        };
        window.WebSocket.prototype = WS.prototype;
        Object.setPrototypeOf(window.WebSocket, WS);
      })();
      window.__uploads = [];
      window.addEventListener('message', (e) => {
        if (e.data && e.data.type === 'fileUpload') window.__uploads.push(e.data.payload);
      });
    """)
    page.goto(f"{H.BASE_URL}/{query}{url_hash}", wait_until="load")
    return browser, page, requests


def wait_upload_end(page, timeout: float = 20) -> list:
    deadline = time.time() + timeout
    while time.time() < deadline:
        ups = page.evaluate("window.__uploads") or []
        if any(u.get("status") in ("end", "error") for u in ups):
            return ups
        time.sleep(0.3)
    return page.evaluate("window.__uploads") or []


def api_cookie(page) -> dict:
    for c in page.context.cookies():
        if c.get("name") == COOKIE:
            return c
    return {}


def run_websockets() -> "H.Results":
    from playwright.sync_api import sync_playwright
    res = H.Results("websockets")
    fresh_files_dir()
    H.server_start(mode="websockets", wayland=False, extra_env=secure_env())
    provision(res)
    with sync_playwright() as pw:
        browser, page, requests = launch(pw, f"?token={CTRL_TOKEN}", "websockets")
        try:
            info = C.wait_ws_video(page, timeout=30)
            res.check("controller page streams over WebSockets with ?token=", info is not None, info)
            c = api_cookie(page)
            res.check("the page set the API cookie from its token",
                      urllib.parse.unquote(c.get("value", "")) == CTRL_TOKEN and c.get("path") == "/api/"
                      and str(c.get("sameSite", "")).lower() == "strict", c)
            page.set_input_files("#globalFileInput", {
                "name": "browser-upload.txt", "mimeType": "text/plain", "buffer": b"from the browser\n"})
            ups = wait_upload_end(page)
            res.check("the upload through the file input completes",
                      any(u.get("status") == "end" for u in ups), ups[-2:])
            res.check("the upload landed in the file manager directory",
                      wait_file(os.path.join(FILES_DIR, "browser-upload.txt"), 5)
                      and open(os.path.join(FILES_DIR, "browser-upload.txt"), "rb").read() == b"from the browser\n")
            up_reqs = [r for r in requests if r[0] == "POST" and "/api/upload" in r[1]]
            res.check("the upload request carried the Bearer token",
                      up_reqs and all(r[2].get("authorization") == f"Bearer {CTRL_TOKEN}" for r in up_reqs),
                      [r[2].get("authorization") for r in up_reqs])

            listing = page.context.new_page()
            listing.goto(f"{H.BASE_URL}/api/files/?token={CTRL_TOKEN}", wait_until="load")
            hrefs = listing.evaluate(
                "() => new Promise(r => setTimeout(() => r([...document.querySelectorAll('table#list td a')].map(a => a.getAttribute('href'))), 800))")
            res.check("the tokened listing renders and its links keep the token",
                      hrefs and any(h.startswith("hello.txt?token=") for h in hrefs)
                      and all(f"token={urllib.parse.quote(CTRL_TOKEN, safe='')}" in h for h in hrefs), hrefs)
            listing.goto(f"{H.BASE_URL}/api/files/", wait_until="load")
            res.check("the bare listing is served on the cookie alone", "hello.txt" in listing.content(), listing.url)
            response = listing.request.get(f"{H.BASE_URL}/api/files/hello.txt")
            res.check("a download on the cookie alone is served",
                      response.status == 200 and response.body() == b"hello from the file manager\n", response.status)
            listing.close()
        finally:
            browser.close()

        browser, page, requests = launch(pw, f"?token={VIEW_TOKEN}", "websockets")
        try:
            info = C.wait_ws_video(page, timeout=30)
            res.check("viewer page streams over WebSockets with ?token=", info is not None, info)
            # The core itself does not upload for a viewer (the token's role
            # puts the page in shared mode); the server is the backstop, so
            # the request is made from the page directly as a script would.
            page.set_input_files("#globalFileInput", {
                "name": "viewer-upload.txt", "mimeType": "text/plain", "buffer": b"nope\n"})
            time.sleep(3)
            res.check("the viewer page's file input starts no upload",
                      not [r for r in requests if r[0] == "POST" and "/api/upload" in r[1]]
                      and not os.path.exists(os.path.join(FILES_DIR, "viewer-upload.txt")))
            status = page.evaluate("""async (token) => {
                const r = await fetch('api/upload', { method: 'POST', body: 'nope',
                    headers: { 'Authorization': 'Bearer ' + token, 'X-Upload-Path': 'viewer-upload.txt' } });
                return r.status;
            }""", VIEW_TOKEN)
            res.check("an upload forced from the viewer page is refused by the server",
                      status == 403 and not os.path.exists(os.path.join(FILES_DIR, "viewer-upload.txt")), status)
            res.check("the refusal did not reload the page",
                      page.evaluate("window.videoChunksReceived || window.__wsFrames") > 0
                      and page.url.endswith(f"?token={VIEW_TOKEN}"), page.url)
        finally:
            browser.close()
    res.summary()
    return res


def run_webrtc() -> "H.Results":
    from playwright.sync_api import sync_playwright
    res = H.Results("webrtc")
    fresh_files_dir()
    H.server_start(mode="webrtc", wayland=False, extra_env=secure_env())
    provision(res)
    status, headers, _ = request("GET", "/api/turn")
    res.check("no token: /api/turn refused", status == 401 and headers.get("www-authenticate") == BEARER_CHALLENGE,
              f"{status} {headers.get('www-authenticate')!r}")
    status, _, body = request("GET", "/api/turn", headers=bearer(CTRL_TOKEN))
    res.check("Bearer controller: /api/turn serves the RTC configuration",
              status == 200 and b"iceServers" in body, f"{status} {body[:60]}")
    status, _, body = request("GET", "/api/turn", headers=cookie(VIEW_TOKEN))
    res.check("cookie viewer: /api/turn serves the RTC configuration", status == 200, status)
    with sync_playwright() as pw:
        browser, page, requests = launch(pw, f"?token={CTRL_TOKEN}", "webrtc")
        try:
            info = C.wait_wr_video(page, timeout=60)
            res.check("controller page streams over WebRTC with ?token=", info is not None, info)
            turn_reqs = [r for r in requests if "/api/turn" in r[1]]
            res.check("the TURN fetch carried the Bearer token",
                      turn_reqs and all(r[2].get("authorization") == f"Bearer {CTRL_TOKEN}" for r in turn_reqs),
                      [r[2].get("authorization") for r in turn_reqs])
            c = api_cookie(page)
            res.check("the WebRTC page set the same API cookie",
                      urllib.parse.unquote(c.get("value", "")) == CTRL_TOKEN and c.get("path") == "/api/", c)
            page.set_input_files("#globalFileInput", {
                "name": "webrtc-upload.txt", "mimeType": "text/plain", "buffer": b"over webrtc\n"})
            ups = wait_upload_end(page)
            res.check("the upload from the WebRTC page completes",
                      any(u.get("status") == "end" for u in ups)
                      and wait_file(os.path.join(FILES_DIR, "webrtc-upload.txt"), 5), ups[-2:])
        finally:
            browser.close()
    res.summary()
    return res


def open_files_modal(page, dashboard: str) -> bool:
    """Click through the dashboard to its Download Files modal."""
    try:
        if dashboard == "classic":
            page.locator('.toggle-handle').first.click()
            time.sleep(0.5)
            page.locator('.sidebar-section-header:has-text("Files")').first.click()
            time.sleep(0.5)
        else:
            triggers = page.locator('[role="menubar"] button')
            opened = False
            for i in range(triggers.count()):
                triggers.nth(i).click()
                time.sleep(0.5)
                item = page.locator('[role="menu"] [role="menuitem"]:has-text("Files")').first
                if item.count():
                    item.click()
                    time.sleep(0.8)
                    opened = True
                    break
                page.keyboard.press("Escape")
                time.sleep(0.2)
            if not opened:
                return False
        page.locator('button:has-text("Download Files")').first.click()
        time.sleep(1.0)
        return True
    except Exception as e:
        print(f"      (files modal: {e!r})")
        return False


def run_dashboards() -> "H.Results":
    from playwright.sync_api import sync_playwright
    res = H.Results("dashboards")
    for dashboard, dist in (("classic", H.CLASSIC_DIST), ("wish", H.WISH_DIST)):
        if not os.path.isfile(os.path.join(dist, "index.html")):
            res.skip(f"{dashboard}: dist not built", dist)
            continue
        fresh_files_dir()
        H.server_start(mode="websockets", wayland=False, web_root=dist,
                       extra_env=secure_env(SELKIES_ENABLE_DUAL_MODE="true"))
        provision(res)
        with sync_playwright() as pw:
            browser, page, requests = launch(pw, f"?token={CTRL_TOKEN}", "websockets")
            navigations = []
            prompts = []

            def on_request(r, page=page, navigations=navigations) -> None:
                if r.is_navigation_request() and r.frame == page.main_frame:
                    navigations.append(r.url)

            def on_dialog(d, prompts=prompts) -> None:
                prompts.append(d.message)
                d.accept(MASTER)

            page.on("request", on_request)
            page.on("dialog", on_dialog)
            try:
                info = C.wait_ws_video(page, timeout=30)
                res.check(f"{dashboard}: the dashboard page streams with ?token=", info is not None, info)
                res.check(f"{dashboard}: the files modal opens", open_files_modal(page, dashboard))
                src = page.evaluate("() => { const f = document.querySelector('iframe'); return f ? f.getAttribute('src') : null; }")
                res.check(f"{dashboard}: the file-manager iframe carries the token",
                          src is not None and "/api/files/" in src and f"token={CTRL_TOKEN}" in src, src)
                listing = None
                deadline = time.time() + 15
                while time.time() < deadline and listing is None:
                    for frame in page.frames:
                        if "/api/files/" in frame.url:
                            try:
                                if "hello.txt" in frame.content():
                                    listing = frame
                            except Exception:
                                pass
                    time.sleep(0.5)
                res.check(f"{dashboard}: the listing rendered inside the modal", listing is not None,
                          [f.url for f in page.frames])
                if listing is not None:
                    hrefs = listing.evaluate("() => [...document.querySelectorAll('table#list td a')].map(a => a.getAttribute('href'))")
                    res.check(f"{dashboard}: the listing's links keep the token",
                              hrefs and all(f"token={CTRL_TOKEN}" in h for h in hrefs), hrefs)
                if dashboard == "classic":
                    # A controller switches on the token its own page was opened
                    # with. Nobody is asked to paste the master token: it is an
                    # operator credential a session user does not hold.
                    page.locator('.files-modal-close').first.click()
                    time.sleep(0.5)
                    page.locator('.sidebar-section-header:has-text("Video")').first.click()
                    time.sleep(0.5)
                    base_navs = len(navigations)
                    page.locator('#streamModeSelect').first.select_option(value="webrtc")
                    switched = False
                    deadline = time.time() + 20
                    while time.time() < deadline and not switched:
                        # Yields to Playwright, so a dialog would reach the handler.
                        page.wait_for_timeout(500)
                        status, _, body = request("GET", "/api/status")
                        switched = status == 200 and json.loads(body).get("current_mode") == "webrtc"
                    res.check("classic: the switch went through on the session token", switched)
                    res.check("classic: nothing asked the user for the master token",
                              not prompts, prompts)
                    res.check("classic: the switch did not reload the page",
                              len(navigations) == base_navs, navigations[base_navs:])
                    switch_reqs = [r for r in requests if r[0] == "POST" and "/api/switch" in r[1]]
                    res.check("classic: the dashboard presents its own token on the switch",
                              switch_reqs and switch_reqs[0][2].get("authorization")
                              == f"Bearer {CTRL_TOKEN}",
                              [r[2].get("authorization") for r in switch_reqs])
            finally:
                browser.close()
    res.summary()
    return res


def run_legacy() -> "H.Results":
    res = H.Results("legacy")
    fresh_files_dir()
    H.server_start(mode="websockets", wayland=False,
                   extra_env={"SELKIES_FILE_MANAGER_PATH": FILES_DIR, "SELKIES_ENABLE_METRICS_HTTP": "true"})
    for method, path in (("GET", "/api/files/"), ("GET", "/api/files/hello.txt"), ("GET", "/api/metrics")):
        status, _, _ = request(method, path, headers=dict(bearer("junk"), **cookie("junk")))
        res.check(f"no master token, Basic off: {method} {path} open", status == 200, status)
    res.check("no master token, Basic off: upload open", upload("legacy.txt", b"l") == 200)

    basic = {"Authorization": "Basic " + base64.b64encode(b"user:secret").decode()}
    viewonly = {"Authorization": "Basic " + base64.b64encode(b"user:look").decode()}
    H.server_start(mode="websockets", wayland=False, extra_env={
        "SELKIES_FILE_MANAGER_PATH": FILES_DIR, "SELKIES_ENABLE_METRICS_HTTP": "true",
        "SELKIES_ENABLE_BASIC_AUTH": "true", "SELKIES_BASIC_AUTH_USER": "user",
        "SELKIES_BASIC_AUTH_PASSWORD": "secret", "SELKIES_BASIC_AUTH_VIEWONLY_PASSWORD": "look"})
    for path in ("/api/files/", "/api/metrics", "/"):
        status, headers, _ = request("GET", path)
        res.check(f"Basic on: GET {path} challenges with Basic",
                  status == 401 and headers.get("www-authenticate", "").startswith("Basic realm="),
                  f"{status} {headers.get('www-authenticate')!r}")
    status, _, _ = request("GET", "/api/files/", headers=bearer(CTRL_TOKEN))
    res.check("Basic on, no master token: a Bearer token is not a credential", status == 401, status)
    status, _, body = request("GET", "/api/files/", headers=basic)
    res.check("Basic on: Basic credentials list files", status == 200 and b"hello.txt" in body, status)
    res.check("Basic on: Basic credentials upload", upload("basic.txt", b"b", basic) == 200)
    res.check("Basic on: the view-only password cannot upload", upload("viewonly.txt", b"v", viewonly) == 403)
    status, _, _ = request("GET", "/api/files/hello.txt", headers=viewonly)
    res.check("Basic on: the view-only password downloads", status == 200, status)
    res.summary()
    return res


BLOCKS = {"routes": run_routes, "websockets": run_websockets, "webrtc": run_webrtc,
          "dashboards": run_dashboards, "legacy": run_legacy}


def main(selectors: list) -> bool:
    ok = True
    for name in selectors or list(BLOCKS):
        try:
            ok = not BLOCKS[name]().failed() and ok
        finally:
            H.server_stop()
    return ok


if __name__ == "__main__":
    sys.exit(0 if main(sys.argv[1:]) else 1)
