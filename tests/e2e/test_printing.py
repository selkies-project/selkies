#!/usr/bin/env python3
"""Printing end to end: a document printed in the session reaches the page.

websockets / webrtc:
    A PDF spooled before the page connects is announced on connect; one
    printed through the session's own CUPS queue (`lp -d Selkies`) while the
    page is up is announced live, fetched whole, taken out of the spool, and,
    with automatic printing on, opened in a frame for the browser's print
    dialog. Both dashboards list each document with its switch, print, and
    save controls; the switch off leaves the next document in the list only,
    and the save link hands the browser the same bytes. A second display page
    of the same browser is told nothing. The websockets block also drives the
    installed Chrome on the test display and sees its print preview open with
    the document.
policy:
    `printing_enabled=false` starts no queue, announces nothing, and refuses
    the route; a shared viewer page is never told of a document.

Usage: python3 tests/e2e/test_printing.py [websockets|webrtc|policy|all]
"""
import base64
import hashlib
import http.client
import os
import shutil
import subprocess
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

SPOOL = os.path.join(H.WORKDIR, "print-spool")
CUPS_SOCKET = os.path.join(H.RUNTIME_DIR, "selkies-cups", "cups.sock")
CUPS_PATH = os.environ.get("PATH", "") + ":/usr/sbin"
SERVER_ENV = {"SELKIES_PRINT_SPOOL_PATH": SPOOL}

# Records every document the core hands the dashboards.
PRINT_JS = """
  window.__printed = [];
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'printDocument') window.__printed.push({ name: e.data.name, size: e.data.size, url: e.data.url });
  });
"""
BLOB_JS = """
  async (url) => {
    const bytes = new Uint8Array(await (await fetch(url)).arrayBuffer());
    let s = ''; for (const b of bytes) s += String.fromCharCode(b);
    return btoa(s);
  }
"""
FRAMES_JS = "document.querySelectorAll('iframe[src^=\"blob:\"]').length"

# Headless Chromium never closes a print dialog, so `afterprint` does not fire
# and the frames the core opens stay, each with a PDF viewer behind it. That
# viewer's process ends when the frame goes or when a download starts, and the
# driver reads either as the page crashing, failing whatever call is in flight.
DROP_FRAMES_JS = ("document.querySelectorAll('iframe[src^=\"blob:\"]')"
                  ".forEach((f) => f.remove())")

# A one-page PDF drawing one rectangle, built by hand.
CONTENT = b"0 0 1 rg 100 100 200 300 re f"
OBJECTS = [b"<< /Type /Catalog /Pages 2 0 R >>",
           b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
           b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R >>",
           b"<< /Length %d >>stream\n" % len(CONTENT) + CONTENT + b"\nendstream"]


def sample_pdf() -> bytes:
    out, offsets = b"%PDF-1.4\n", []
    for i, obj in enumerate(OBJECTS, 1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % i + obj + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(OBJECTS) + 1)
    out += b"".join(b"%010d 00000 n \n" % o for o in offsets)
    return out + b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(OBJECTS) + 1, xref)


PDF = sample_pdf()


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def fresh_spool() -> None:
    shutil.rmtree(SPOOL, ignore_errors=True)
    os.makedirs(SPOOL)


def spool(name: str, data: bytes = PDF) -> str:
    """A document dropped into the spool the way the backend does it."""
    part = os.path.join(SPOOL, f".{name}.part")
    with open(part, "wb") as f:
        f.write(data)
    os.replace(part, os.path.join(SPOOL, name))
    return os.path.join(SPOOL, name)


def queue_available() -> bool:
    return os.path.exists(CUPS_SOCKET) and bool(shutil.which("lp", path=CUPS_PATH))


def lp(title: str, path: str) -> bool:
    """Print `path` to the session's Selkies queue under `title`."""
    env = {"PATH": CUPS_PATH, "CUPS_SERVER": CUPS_SOCKET, "HOME": H.WORKDIR}
    return subprocess.run(["lp", "-d", "Selkies", "-t", title, path], env=env,
                          capture_output=True, timeout=60).returncode == 0


def route_status(name: str) -> int:
    """The status `/api/print/<name>` answers with, refusals included."""
    conn = http.client.HTTPConnection("localhost", H.PORT, timeout=10)
    try:
        conn.request("GET", "/api/print/" + urllib.parse.quote(name))
        return conn.getresponse().status
    finally:
        conn.close()


def wait_printed(page: Any, count: int, timeout: float = 30) -> list:
    deadline = time.time() + timeout
    while time.time() < deadline:
        got = page.evaluate("window.__printed") or []
        if len(got) >= count:
            return got
        time.sleep(0.25)
    return page.evaluate("window.__printed") or []


def wait_gone(path: str, timeout: float = 10) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if not os.path.exists(path):
            return True
        time.sleep(0.2)
    return not os.path.exists(path)


def blob_bytes(page: Any, url: str) -> bytes:
    try:
        return base64.b64decode(page.evaluate(BLOB_JS, url))
    except Exception as e:
        print(f"      (blob fetch: {e!r})")
        return b""


def blob_matches(page: Any, url: str, data: bytes) -> bool:
    return blob_bytes(page, url) == data


def blob_prefix(page: Any, url: str) -> bytes:
    return blob_bytes(page, url)[:4]


def new_page(pw: Any, mode: str, url_hash: str = "", headed_chrome: bool = False, **context: Any) -> tuple:
    """A page on the dashboard served at web_root with the print recorder
    installed; the installed Chrome on the test display when `headed_chrome`."""
    if headed_chrome:
        browser = pw.chromium.launch(headless=False, executable_path=shutil.which("google-chrome"),
                                     args=C.BROWSER_ARGS, env={**os.environ, "DISPLAY": H.require_display()})
    else:
        browser = C.chromium_launch(pw)
    ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=1, **context)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(PRINT_JS)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return browser, page


def wait_video(page: Any, mode: str) -> Optional[dict]:
    return C.wait_wr_video(page, timeout=45) if mode == "webrtc" else C.wait_ws_video(page, timeout=30)


def open_printing_ui(page: Any, dashboard: str) -> bool:
    """Reach the printing controls: the classic sidebar section, or the wish
    menubar's Printing submenu."""
    if dashboard == "classic":
        try:
            if page.locator("#printing-content").count() == 0:
                page.locator(".toggle-handle").first.click()
                time.sleep(0.6)
            if page.locator("#printing-content").count() == 0:
                page.locator('.sidebar-section-header:has-text("Printing")').first.click()
                time.sleep(0.6)
            return page.locator("#printing-content").count() > 0
        except Exception as e:
            print(f"      (classic printing section: {e!r})")
            return False
    for _ in range(4):
        if page.locator('[role="menu"]').count() == 0:
            break
        page.keyboard.press("Escape")
        time.sleep(0.3)
    return TD.wish_open_menu_item(page, "Printing")


def rows(page: Any, dashboard: str) -> int:
    """Documents listed: one save link each, whatever else the row offers."""
    if dashboard == "classic":
        return page.locator("#printing-content .print-job").count()
    return page.locator('[role="menu"] a[download]').count()


def drop_frames(page: Any, timeout: float = 10) -> None:
    """Drop the preview frames and wait for their viewers to go, so the
    teardown lands between calls rather than inside the next one."""
    page.evaluate(DROP_FRAMES_JS)
    deadline = time.time() + timeout
    while time.time() < deadline and page.evaluate(FRAMES_JS):
        time.sleep(0.2)
    time.sleep(1)


def press(control: Any) -> None:
    """Click a control; the classic sidebar scrolls, so a section below the
    fold takes a dispatched click rather than a pointer one."""
    control.dispatch_event("click")
    time.sleep(0.5)


def click_switch(page: Any, dashboard: str) -> None:
    if dashboard == "classic":
        press(page.locator("#printAutoToggle"))
    else:
        press(page.locator('[role="menu"] [role="switch"]').first)


def row_control(page: Any, dashboard: str, label: str, index: int = 0) -> Any:
    """A document row's icon control, named by its title ("Print" or "Save")."""
    if dashboard == "classic":
        rows_ = page.locator("#printing-content .print-job")
        return rows_.nth(index).locator(f'[title="{label}"]')
    return page.locator(f'[role="menu"] :is(button, a)[title="{label}"]').nth(index)


def print_preview_open(browser: Any) -> bool:
    """Whether Chrome's print preview is showing a PDF: its own page and the
    document it renders are targets of the browser."""
    cdp = browser.new_browser_cdp_session()
    for _ in range(40):
        urls = [t["url"] for t in cdp.send("Target.getTargets")["targetInfos"]]
        if any(u.startswith("chrome://print") for u in urls) and any("print.pdf" in u for u in urls):
            return True
        time.sleep(0.25)
    return False


def dashboard_round(res: "H.Results", pw: Any, mode: str, dashboard: str) -> None:
    fresh_spool()
    pending = spool("Pending.pdf")
    H.server_start(mode=mode, wayland=False, web_root=H.CLASSIC_DIST if dashboard == "classic" else H.WISH_DIST,
                   extra_env=SERVER_ENV)
    browser, page = new_page(pw, mode)
    try:
        res.check(f"{dashboard}: video streams over {mode}", wait_video(page, mode) is not None)
        got = wait_printed(page, 1)
        res.check(f"{dashboard}: a document spooled before the connection is announced on connect",
                  [d["name"] for d in got] == ["Pending.pdf"], got)
        res.check(f"{dashboard}: the page holds the document's bytes",
                  got and blob_matches(page, got[0]["url"], PDF))
        res.check(f"{dashboard}: the document left the spool once taken", wait_gone(pending))
        res.check(f"{dashboard}: automatic printing opened it in a frame",
                  page.evaluate(FRAMES_JS) == 1, page.evaluate(FRAMES_JS))
        res.check(f"{dashboard}: the printing controls appear with the document",
                  open_printing_ui(page, dashboard) and rows(page, dashboard) == 1, rows(page, dashboard))

        click_switch(page, dashboard)
        if queue_available():
            res.check(f"{dashboard}: lp prints to the Selkies queue", lp("Quarterly report", write_sample("quarterly.pdf")))
        else:
            res.skip(f"{dashboard}: lp prints to the Selkies queue", "no cupsd on this host, spooling directly")
            spool("Quarterly report.pdf")
        got = wait_printed(page, 2)
        res.check(f"{dashboard}: a document printed while connected is announced live as a PDF",
                  [d["name"] for d in got] == ["Pending.pdf", "Quarterly report.pdf"]
                  and blob_prefix(page, got[1]["url"]) == b"%PDF", got)
        res.check(f"{dashboard}: with automatic printing off it waits in the list",
                  page.evaluate(FRAMES_JS) == 1 and open_printing_ui(page, dashboard) and rows(page, dashboard) == 2,
                  (page.evaluate(FRAMES_JS), rows(page, dashboard)))
        press(row_control(page, dashboard, "Print", 1))
        time.sleep(1)
        res.check(f"{dashboard}: the print button opens it in a frame", page.evaluate(FRAMES_JS) == 2, page.evaluate(FRAMES_JS))
        drop_frames(page)
        # The wish panel is a menu, and printing from it closed it.
        open_printing_ui(page, dashboard)
        with page.expect_download(timeout=15000) as dl:
            press(row_control(page, dashboard, "Save", 0))
        download = dl.value
        saved = download.path()
        res.check(f"{dashboard}: the save link hands the browser the same bytes under the document's name",
                  download.suggested_filename == "Pending.pdf" and saved and sha(open(saved, "rb").read()) == sha(PDF),
                  download.suggested_filename)

        second = browser.contexts[0].new_page()
        second.goto(H.BASE_URL + "/#display2", wait_until="load")
        time.sleep(3)
        spool("Third.pdf")
        got = wait_printed(page, 3)
        time.sleep(1)
        res.check(f"{dashboard}: a second display page of the same browser is told nothing",
                  len(got) == 3 and (second.evaluate("window.__printed") or []) == [],
                  second.evaluate("window.__printed"))
        second.close()
    finally:
        browser.close()


def write_sample(name: str) -> str:
    """The sample PDF written outside the spool, for lp to read."""
    path = os.path.join(H.WORKDIR, name)
    with open(path, "wb") as f:
        f.write(PDF)
    return path


def transport_block(mode: str) -> "H.Results":
    res = H.Results(f"printing-{mode}")
    with sync_playwright() as pw:
        for dashboard in ("classic", "wish"):
            dashboard_round(res, pw, mode, dashboard)
        if mode == "websockets":
            if shutil.which("google-chrome") and H.TEST_DISPLAY:
                fresh_spool()
                spool("Preview.pdf")
                H.server_start(mode=mode, wayland=False, web_root=H.CLASSIC_DIST, extra_env=SERVER_ENV)
                browser, page = new_page(pw, mode, headed_chrome=True)
                try:
                    res.check("Chrome: video streams", wait_video(page, mode) is not None)
                    res.check("Chrome: the document arrives", len(wait_printed(page, 1)) == 1)
                    res.check("Chrome: the browser's print preview opens with the document", print_preview_open(browser))
                finally:
                    browser.close()
            else:
                res.skip("Chrome: the browser's print preview opens with the document", "google-chrome or the test display is missing")
    res.summary()
    return res


def policy_block() -> "H.Results":
    res = H.Results("printing-policy")
    with sync_playwright() as pw:
        fresh_spool()
        spool("Off.pdf")
        H.server_start(mode="websockets", wayland=False, web_root=H.CLASSIC_DIST,
                       extra_env={**SERVER_ENV, "SELKIES_PRINTING_ENABLED": "false"})
        res.check("printing off: no queue is started", not os.path.exists(CUPS_SOCKET))
        res.check("printing off: the route refuses", route_status("Off.pdf") == 403, route_status("Off.pdf"))
        browser, page = new_page(pw, "websockets")
        try:
            res.check("printing off: the page streams", wait_video(page, "websockets") is not None)
            time.sleep(3)
            res.check("printing off: nothing is announced and the document stays put",
                      (page.evaluate("window.__printed") or []) == [] and os.path.exists(os.path.join(SPOOL, "Off.pdf")))
            res.check("printing off: no printing section",
                      page.locator('.sidebar-section-header:has-text("Printing")').count() == 0)
        finally:
            browser.close()

        fresh_spool()
        H.server_start(mode="websockets", wayland=False, web_root=H.CLASSIC_DIST, extra_env=SERVER_ENV)
        owner_browser, owner = new_page(pw, "websockets")
        viewer_browser, viewer = new_page(pw, "websockets", url_hash="#shared")
        try:
            res.check("the owner page streams", wait_video(owner, "websockets") is not None)
            res.check("the viewer page streams", wait_video(viewer, "websockets") is not None)
            spool("Owner.pdf")
            got = wait_printed(owner, 1)
            time.sleep(2)
            res.check("a document goes to the owner and never to a shared viewer",
                      [d["name"] for d in got] == ["Owner.pdf"] and (viewer.evaluate("window.__printed") or []) == [],
                      viewer.evaluate("window.__printed"))
        finally:
            owner_browser.close()
            viewer_browser.close()
    res.summary()
    return res


def main() -> None:
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
    print(f"\n=== PRINTING: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
