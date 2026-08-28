#!/usr/bin/env python3
"""File transfer end to end: uploads into `file_manager_path`, downloads out of
it, on both transports and under every `file_transfers` policy value.

websockets / webrtc:
    The server's upload and download routes, driven the way the client drives
    them (POST /api/upload with the X-Upload-Path header and the chunked
    X-Upload-* headers; GET /api/files/<name> and the directory index), with
    the bytes compared on disk and on the wire; then both dashboards on a real
    page: the Files UI's upload button feeds the browser's file chooser, the
    upload lands in the directory, the Download Files modal lists it inside
    its iframe, and both the link it renders and a real click on it hand the
    browser the same bytes. The websockets block also drives one file above the
    client's slicing threshold through the page so the client's chunked path is
    exercised end to end.
policy:
    `file_transfers=upload`, `download` and `none` each refuse the other
    direction on the wire and hide its button in both dashboards; with Basic
    auth the view-only password (the server's viewer ceiling) downloads but
    cannot upload, and a shared/viewer page never starts an upload at all.

Usage: python3 tests/e2e/test_file_transfer.py [websockets|webrtc|policy|all]
"""
import base64
import hashlib
import http.client
import os
import shutil
import sys
import time
import urllib.parse
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_dashboards as TD
from playwright.sync_api import sync_playwright

FILES_DIR = os.path.join(H.WORKDIR, "file-manager")
# One byte over the client's UPLOAD_CHUNK_BYTES: the smallest file the page
# slices into two POSTs.
CHUNKED_BYTES = 64 * 1024 * 1024 + 1
BASIC_USER, BASIC_PASSWORD, VIEWONLY_PASSWORD = "user", "secret", "look"

# Records the core's upload progress messages, so a check can wait for the end
# of a transfer the page started.
UPLOAD_JS = """
  window.__uploads = [];
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'fileUpload') window.__uploads.push(e.data.payload);
  });
"""


def request(method: str, path: str, headers: Optional[dict] = None,
            body: Optional[bytes] = None) -> tuple:
    """One request against the test server.

    Returns:
        `(status, lower-cased response headers, body bytes)`; 4xx/5xx are
        returned, not raised.
    """
    conn = http.client.HTTPConnection("localhost", H.PORT, timeout=60)
    try:
        conn.request(method, path, body=body, headers=dict(headers or {}))
        response = conn.getresponse()
        return response.status, {k.lower(): v for k, v in response.getheaders()}, response.read()
    finally:
        conn.close()


def client_upload(name: str, data: bytes, extra: Optional[dict] = None,
                  auth: Optional[dict] = None) -> tuple:
    """POST /api/upload with the headers lib/file-upload.js sends.

    Args:
        name: Destination path relative to the file-manager root.
        data: Request body.
        extra: The chunked-transfer headers, when slicing.
        auth: Credentials header.

    Returns:
        `(status, body bytes)`.
    """
    headers = {"Content-Type": "application/octet-stream",
               "X-Upload-Path": urllib.parse.quote(name)}
    headers.update(extra or {})
    headers.update(auth or {})
    status, _, body = request("POST", "/api/upload", headers=headers, body=data)
    return status, body


def basic(password: str) -> dict:
    token = base64.b64encode(f"{BASIC_USER}:{password}".encode()).decode()
    return {"Authorization": f"Basic {token}"}


def fresh_files_dir() -> None:
    """An empty file-manager directory seeded with one downloadable file."""
    shutil.rmtree(FILES_DIR, ignore_errors=True)
    os.makedirs(FILES_DIR)
    with open(os.path.join(FILES_DIR, "seed.txt"), "wb") as f:
        f.write(b"seeded before the server started\n")


def on_disk(rel: str) -> str:
    return os.path.join(FILES_DIR, rel)


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def file_sha(path: str) -> Optional[str]:
    try:
        with open(path, "rb") as f:
            return sha(f.read())
    except OSError:
        return None


def wait_file(path: str, timeout: float = 15) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if os.path.exists(path):
            return True
        time.sleep(0.25)
    return False


def staging_leftovers() -> list:
    """Hidden staging files anywhere under the file-manager directory: a
    finished or discarded transfer must leave none behind."""
    found = []
    for root, _, names in os.walk(FILES_DIR):
        found.extend(os.path.join(root, n) for n in names if n.startswith(".selkies-upload-"))
    return found


def api_checks(res: "H.Results", full: bool) -> None:
    """The HTTP routes as the client uses them.

    Args:
        res: Results accumulator.
        full: Also cover slicing, the refusals and the index details; the
            other transport's block repeats only the plain round trip, since
            the routes are transport-independent.
    """
    payload = os.urandom(1 << 20)
    status, body = client_upload("sub dir/plain.bin", payload)
    res.check("plain POST /api/upload accepted", status == 200 and b'"success"' in body, f"{status} {body[:60]}")
    res.check("the plain upload landed byte for byte",
              wait_file(on_disk("sub dir/plain.bin"), 5) and file_sha(on_disk("sub dir/plain.bin")) == sha(payload))
    status, headers, body = request("GET", "/api/files/sub%20dir/plain.bin")
    res.check("GET /api/files/<name> serves the same bytes as an attachment",
              status == 200 and sha(body) == sha(payload)
              and "attachment" in headers.get("content-disposition", ""),
              f"{status} {headers.get('content-disposition')}")
    status, _, body = request("GET", "/api/files/")
    res.check("the file index lists the root", status == 200 and b"seed.txt" in body and b"sub dir/" in body,
              f"{status} {body[:40]}")
    if not full:
        return

    # The client slices above its threshold; three slices of this transfer
    # exercise the create / append / finalize legs of the server's .part path.
    slices = [os.urandom(300 * 1024), os.urandom(200 * 1024), os.urandom(100 * 1024)]
    total = sum(len(s) for s in slices)
    offset = 0
    statuses = []
    for i, part in enumerate(slices):
        extra = {"X-Upload-Id": "e2e-transfer-1", "X-Upload-Offset": str(offset),
                 "X-Upload-Total": str(total)}
        if i == len(slices) - 1:
            extra["X-Upload-Final"] = "1"
        status, body = client_upload("chunked.bin", part, extra)
        statuses.append((status, b'"complete": true' in body or b'"complete":true' in body))
        offset += len(part)
    res.check("chunked slices accepted, only the final one completes",
              [s for s, _ in statuses] == [200, 200, 200] and [c for _, c in statuses] == [False, False, True],
              statuses)
    res.check("the chunked upload is the concatenation of its slices",
              file_sha(on_disk("chunked.bin")) == sha(b"".join(slices)))
    res.check("no staging file is left behind", not staging_leftovers(), staging_leftovers())
    status, _ = client_upload("broken.bin", b"first", {"X-Upload-Id": "e2e-transfer-2", "X-Upload-Offset": "0",
                                                       "X-Upload-Total": "10"})
    status2, body = client_upload("broken.bin", b"later", {"X-Upload-Id": "e2e-transfer-2", "X-Upload-Offset": "9",
                                                           "X-Upload-Total": "10", "X-Upload-Final": "1"})
    res.check("a slice at the wrong offset is refused with 409 and the transfer discarded",
              status == 200 and status2 == 409 and not os.path.exists(on_disk("broken.bin"))
              and not staging_leftovers(), f"{status} {status2} {body[:60]}")
    status, _ = client_upload("x.bin", b"x", {"X-Upload-Id": "lonely"})
    res.check("X-Upload-Id without X-Upload-Offset is a 400", status == 400, status)
    status, _ = client_upload("../escape.bin", b"x")
    escaped = os.path.exists(os.path.join(os.path.dirname(FILES_DIR), "escape.bin"))
    res.check("a path that escapes the root is refused", status == 400 and not escaped, status)

    status, headers, body = request("GET", "/api/files/sub%20dir/plain.bin", headers={"Range": "bytes=100-199"})
    res.check("a Range request is answered with the slice",
              status == 206 and body == payload[100:200], f"{status} {len(body)}")
    status, headers, _ = request("HEAD", "/api/files/sub%20dir/plain.bin")
    res.check("HEAD carries the length without a body",
              status == 200 and headers.get("content-length") == str(len(payload)), f"{status} {headers.get('content-length')}")
    status, headers, _ = request("GET", "/api/files/sub%20dir")
    res.check("a directory without its slash redirects to it",
              status in (301, 308) and headers.get("location", "").endswith("/api/files/sub%20dir/"),
              f"{status} {headers.get('location')}")
    status, _, body = request("GET", "/api/files/sub%20dir/")
    res.check("a subdirectory index lists its files and the parent link",
              status == 200 and b"plain.bin" in body and b"../" in body, status)
    status, _, _ = request("GET", "/api/files/missing.bin")
    res.check("a missing file is a 404", status == 404, status)
    status, _, _ = request("GET", "/api/files/../seed.txt")
    res.check("a traversal in the download path is refused", status in (403, 404), status)


def close_menus(page: Any) -> None:
    """Close whatever Radix menu is open so the next open starts from a closed
    menubar (a trigger click on an open menu would close it instead)."""
    for _ in range(4):
        if page.locator('[role="menu"]').count() == 0:
            return
        page.keyboard.press("Escape")
        time.sleep(0.3)


def open_files_ui(page: Any, dashboard: str) -> bool:
    """Reach the Files controls: the classic sidebar section, or the Wish
    menubar's Files submenu."""
    if dashboard == "classic":
        try:
            page.locator('.toggle-handle').first.click()
            time.sleep(0.6)
            page.locator('.sidebar-section-header:has-text("Files")').first.click()
            time.sleep(0.6)
            return page.locator('#files-content').count() > 0
        except Exception as e:
            print(f"      (classic files section: {e!r})")
            return False
    close_menus(page)
    return TD.wish_open_menu_item(page, "Files")


def files_buttons(page: Any) -> tuple:
    """`(upload shown, download shown)` among the visible buttons."""
    return (page.locator('button:has-text("Upload Files")').count() > 0,
            page.locator('button:has-text("Download Files")').count() > 0)


def wait_upload_end(page: Any, name: str, timeout: float = 60,
                    watch: Optional[Any] = None) -> list:
    """The page's upload messages once `name` reports end or error.

    Args:
        page: The dashboard page.
        name: File whose transfer is being waited on.
        timeout: Seconds to wait for it to finish.
        watch: Called on every poll, for a caller sampling what the dashboard
            shows while the transfer is still running.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        if watch is not None:
            watch()
        ups = page.evaluate("window.__uploads") or []
        if any(u.get("fileName") == name and u.get("status") in ("end", "error") for u in ups):
            return ups
        time.sleep(0.3)
    return page.evaluate("window.__uploads") or []


def upload_progress_shown(page: Any, dashboard: str, name: str) -> bool:
    """Whether the dashboard is showing this upload's progress at this moment.

    A transfer with no visible progress reads as a dashboard that dropped it,
    which is the whole reason the notification exists; each dashboard has its
    own: a progress bar in the classic sidebar, a toast in wish.
    """
    if dashboard == "classic":
        return page.locator(".notification-progress-bar-inner").count() > 0
    return page.locator(f'[data-sonner-toast]:has-text("{name}")').count() > 0


def dashboard_page(pw: Any, mode: str, url_hash: str = "", **context: Any) -> tuple:
    """A Chromium page on the dashboard served at web_root, with the upload
    recorder installed. Returns `(browser, page)`."""
    browser = C.chromium_launch(pw)
    ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=1, **context)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(UPLOAD_JS)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return browser, page


def wait_video(page: Any, mode: str) -> Optional[dict]:
    return C.wait_wr_video(page, timeout=45) if mode == "webrtc" else C.wait_ws_video(page, timeout=30)


def ui_round_trip(res: "H.Results", page: Any, dashboard: str, mode: str) -> None:
    """Upload through the dashboard's button and download through its modal."""
    name = f"{dashboard}-{mode}.bin"
    payload = os.urandom(256 * 1024)
    res.check(f"{dashboard}: the Files UI opens", open_files_ui(page, dashboard))
    chosen = False
    try:
        with page.expect_file_chooser(timeout=8000) as chooser:
            page.locator('button:has-text("Upload Files")').first.click()
        chooser.value.set_files({"name": name, "mimeType": "application/octet-stream", "buffer": payload})
        chosen = True
    except Exception as e:
        print(f"      (file chooser: {e!r})")
    res.check(f"{dashboard}: Upload Files opens the browser's file chooser", chosen)
    ups = wait_upload_end(page, name)
    res.check(f"{dashboard}: the upload reports its end to the dashboard",
              any(u.get("fileName") == name and u.get("status") == "end" for u in ups), ups[-2:])
    res.check(f"{dashboard}: the upload landed in file_manager_path byte for byte",
              wait_file(on_disk(name), 5) and file_sha(on_disk(name)) == sha(payload))

    def listing_frame(seconds: float):
        """The modal's index frame once it lists `name`, or None."""
        # The index shares the server's loop with the encoder; starved runners lag it.
        deadline = time.time() + seconds
        while time.time() < deadline:
            for frame in page.frames:
                if "/api/files/" in frame.url:
                    try:
                        if name in frame.content():
                            return frame
                    except Exception:
                        pass
            time.sleep(0.5)
        return None

    listing = None
    # The chooser takes the focus, which folds the classic sidebar and closes
    # the Wish submenu the button sits in, so the controls are reopened before
    # reaching for the second button — and again if the click found nothing.
    for attempt in (0, 1):
        if attempt or dashboard == "wish":
            open_files_ui(page, dashboard)
        try:
            page.locator('button:has-text("Download Files")').first.click()
        except Exception as e:
            print(f"      (download button: {e!r})")
        listing = listing_frame(30 if attempt == 0 else 15)
        if listing is not None:
            break
    res.check(f"{dashboard}: the Download Files modal lists the upload", listing is not None,
              [f.url for f in page.frames])
    fetched = None
    href = None
    if listing is not None:
        link = listing.locator(f'table#list td a[href^="{name}"]').first
        href = link.get_attribute("href") if link.count() else None
        if href:
            response = page.request.get(urllib.parse.urljoin(listing.url, href))
            fetched = sha(response.body()) if response.status == 200 else f"status {response.status}"
    res.check(f"{dashboard}: the listing's link serves the same bytes", fetched == sha(payload), f"{href} -> {fetched}")
    # A real click focuses the iframe, which blurs the window; Radix closes every
    # menu on that, tearing down a modal mounted inside a menubar submenu.
    downloaded = None
    detail = ""
    if listing is not None and href:
        try:
            with page.expect_download(timeout=20000) as dl:
                listing.locator(f'table#list td a[href="{href}"]').first.click()
            path = dl.value.path()
            downloaded = file_sha(path) if path else None
        except Exception as e:
            detail = f"no download: {str(e).splitlines()[0][:60]}"
        if listing.is_detached():
            detail += "; the modal closed on the click"
    res.check(f"{dashboard}: a real click on the listing link downloads the same bytes",
              downloaded == sha(payload), detail)


def transport_block(mode: str) -> "H.Results":
    """Routes and both dashboards' file UI on one transport."""
    res = H.Results(f"files-{mode}")
    fresh_files_dir()
    H.server_start(mode=mode, wayland=False, extra_env={"SELKIES_FILE_MANAGER_PATH": FILES_DIR})
    api_checks(res, full=(mode == "websockets"))
    for dashboard, dist in (("classic", H.CLASSIC_DIST), ("wish", H.WISH_DIST)):
        H.server_start(mode=mode, wayland=False, web_root=dist,
                       extra_env={"SELKIES_FILE_MANAGER_PATH": FILES_DIR})
        with sync_playwright() as pw:
            browser, page = dashboard_page(pw, mode)
            try:
                info = wait_video(page, mode)
                res.check(f"{dashboard}: video streams over {mode}", info is not None, info)
                ui_round_trip(res, page, dashboard, mode)
                if mode == "websockets":
                    chunked_through_page(res, page, dashboard)
            finally:
                browser.close()
    res.summary()
    return res


def chunked_through_page(res: "H.Results", page: Any, dashboard: str) -> None:
    """One file above the client's slicing threshold, set on the core's file
    input (what the Upload Files button's chooser feeds): the page posts it as
    slices and the server reassembles it, reporting its progress as it goes."""
    src = os.path.join(H.WORKDIR, "chunked-src.bin")
    with open(src, "wb") as f:
        for _ in range(CHUNKED_BYTES // (1 << 20)):
            f.write(os.urandom(1 << 20))
        f.write(os.urandom(CHUNKED_BYTES % (1 << 20)))
    requests = []
    page.on("request", lambda r: requests.append(r) if r.method == "POST" and "/api/upload" in r.url else None)
    started = time.time()
    page.set_input_files("#globalFileInput", src)
    shown = []

    def sample() -> None:
        if not shown and upload_progress_shown(page, dashboard, "chunked-src.bin"):
            shown.append(time.time() - started)

    ups = wait_upload_end(page, "chunked-src.bin", timeout=180, watch=sample)
    elapsed = time.time() - started
    res.check(f"{dashboard}: the transfer is on screen while it runs", bool(shown),
              f"first seen after {shown[0]:.1f}s of {elapsed:.1f}s" if shown else "never shown")
    res.check("a file above the slicing threshold uploads from the page",
              any(u.get("fileName") == "chunked-src.bin" and u.get("status") == "end" for u in ups),
              f"{elapsed:.1f}s {ups[-1:]}")
    slices = [r for r in requests if r.headers.get("x-upload-id")]
    offsets = sorted(int(r.headers.get("x-upload-offset", -1)) for r in slices)
    res.check("the page sliced it into sequential chunk POSTs",
              len(slices) == 2 and offsets == [0, CHUNKED_BYTES - 1]
              and all(r.headers.get("x-upload-total") == str(CHUNKED_BYTES) for r in slices), offsets)
    res.check("the reassembled file matches the source",
              wait_file(on_disk("chunked-src.bin"), 10) and file_sha(on_disk("chunked-src.bin")) == file_sha(src)
              and not staging_leftovers())
    os.remove(src)


def policy_block() -> "H.Results":
    """`file_transfers` values and the viewer ceiling."""
    res = H.Results("files-policy")
    for value, upload_ok, download_ok in (("upload", True, False), ("download", False, True), ("none", False, False)):
        for dashboard, dist in (("classic", H.CLASSIC_DIST), ("wish", H.WISH_DIST)):
            fresh_files_dir()
            H.server_start(mode="websockets", wayland=False, web_root=dist,
                           extra_env={"SELKIES_FILE_MANAGER_PATH": FILES_DIR, "SELKIES_FILE_TRANSFERS": value})
            if dashboard == "classic":
                status, _ = client_upload(f"{value}.bin", b"policy")
                landed = os.path.exists(on_disk(f"{value}.bin"))
                res.check(f"file_transfers={value}: upload {'accepted' if upload_ok else 'refused'}",
                          (status == 200 and landed) if upload_ok else (status == 403 and not landed), status)
                status, _, body = request("GET", "/api/files/seed.txt")
                res.check(f"file_transfers={value}: download {'served' if download_ok else 'refused'}",
                          (status == 200 and body.startswith(b"seeded")) if download_ok else status == 403, status)
                status, _, _ = request("GET", "/api/files/")
                res.check(f"file_transfers={value}: the index follows the download permission",
                          status == (200 if download_ok else 403), status)
            with sync_playwright() as pw:
                browser, page = dashboard_page(pw, "websockets")
                try:
                    res.check(f"file_transfers={value}: {dashboard} streams", wait_video(page, "websockets") is not None)
                    opened = open_files_ui(page, dashboard)
                    up, down = files_buttons(page)
                    if value == "none":
                        # Wish drops the Files entry entirely; classic keeps the
                        # section but renders neither button.
                        res.check(f"file_transfers=none: {dashboard} offers neither button",
                                  not up and not down, f"opened={opened} upload={up} download={down}")
                    else:
                        res.check(f"file_transfers={value}: {dashboard} shows only the allowed button",
                                  opened and up == upload_ok and down == download_ok,
                                  f"upload={up} download={down}")
                finally:
                    browser.close()

    fresh_files_dir()
    H.server_start(mode="websockets", wayland=False, web_root=H.CLASSIC_DIST, extra_env={
        "SELKIES_FILE_MANAGER_PATH": FILES_DIR, "SELKIES_ENABLE_BASIC_AUTH": "true",
        "SELKIES_BASIC_AUTH_USER": BASIC_USER, "SELKIES_BASIC_AUTH_PASSWORD": BASIC_PASSWORD,
        "SELKIES_BASIC_AUTH_VIEWONLY_PASSWORD": VIEWONLY_PASSWORD})
    status, _ = client_upload("anon.bin", b"x")
    res.check("Basic auth: an upload without credentials is challenged", status == 401, status)
    status, _ = client_upload("controller.bin", b"controller", auth=basic(BASIC_PASSWORD))
    res.check("Basic auth: the main password uploads",
              status == 200 and wait_file(on_disk("controller.bin"), 5), status)
    status, _ = client_upload("viewer.bin", b"viewer", auth=basic(VIEWONLY_PASSWORD))
    res.check("Basic auth: the view-only password cannot upload",
              status == 403 and not os.path.exists(on_disk("viewer.bin")), status)
    status, _, body = request("GET", "/api/files/controller.bin", headers=basic(VIEWONLY_PASSWORD))
    res.check("Basic auth: the view-only password downloads", status == 200 and body == b"controller", status)
    status, _, body = request("GET", "/api/files/", headers=basic(VIEWONLY_PASSWORD))
    res.check("Basic auth: the view-only password lists files", status == 200 and b"controller.bin" in body, status)

    # The client side of the ceiling: the core closes the upload gate on a
    # shared/viewer page, so its file input starts no request at all.
    with sync_playwright() as pw:
        browser, page = dashboard_page(pw, "websockets", url_hash="#shared", http_credentials={
            "username": BASIC_USER, "password": BASIC_PASSWORD})
        try:
            res.check("a shared viewer page streams", wait_video(page, "websockets") is not None)
            posts = []
            page.on("request", lambda r: posts.append(r.url) if r.method == "POST" and "/api/upload" in r.url else None)
            page.set_input_files("#globalFileInput", {
                "name": "shared.bin", "mimeType": "application/octet-stream", "buffer": b"shared"})
            time.sleep(3)
            res.check("the shared page's file input starts no upload",
                      not posts and not os.path.exists(on_disk("shared.bin")), posts)
        finally:
            browser.close()
    res.summary()
    return res


def main() -> None:
    """Run the blocks named on argv (default: all)."""
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    try:
        if which in ("all", "websockets"):
            blocks.append(transport_block("websockets"))
        if which in ("all", "webrtc"):
            blocks.append(transport_block("webrtc"))
        if which in ("all", "policy"):
            blocks.append(policy_block())
    finally:
        H.server_stop()
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== FILES: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
