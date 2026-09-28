#!/usr/bin/env python3
"""A password copied in the session stays out of sight on the page, and off the
local clipboard once the session is done with it.

KeePassXC and KDE offer `x-kde-passwordManagerHint` = `secret` beside a
password they copy, and a toolkit that validates mime types sees it only under
a `text/` or `application/` prefix. The page shows such a copy masked in both
dashboards' clipboard panes, with a button that copies it, and keeps nothing of
it in storage or in the document. With seamless sync it writes it to the local
clipboard and takes it back 60 seconds later, or as soon as the session's
clipboard lets it go, but only while the local clipboard still holds it. That
check runs where the engine allows one without a gesture or a prompt, Chromium
with clipboard-read granted; Firefox and WebKit read nothing for it, which is
checked as well. The session's copy comes from an X client offering the hint,
or on Wayland from a data-control source on the capture compositor.
Usage: python3 tests/e2e/test_clipboard_secret.py [websockets|webrtc|wayland|engines|all]
"""
import base64
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
import test_dashboards as TD  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

HINTS = ("x-kde-passwordManagerHint", "text/x-kde-passwordManagerHint",
         "application/x-kde-passwordManagerHint")
MASK = "•" * 8
WL_SOCKET = "wayland-1"
# The core's term for a secret on the local clipboard, plus the slack a
# delivery and the check take.
TERM_S = 60.0

TAP_JS = """(() => {
  window.__clipMsgs = [];
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
  });
  window.__clipReads = 0;
  window.__clipWrites = [];
  if (navigator.clipboard && navigator.clipboard.readText) {
    const read = navigator.clipboard.readText.bind(navigator.clipboard);
    Object.defineProperty(navigator.clipboard, 'readText', { configurable: true,
      value: (...args) => { window.__clipReads++; return read(...args); } });
    const write = navigator.clipboard.writeText.bind(navigator.clipboard);
    Object.defineProperty(navigator.clipboard, 'writeText', { configurable: true,
      value: (text) => write(text).then(
        () => { window.__clipWrites.push([text, 'ok']); },
        (err) => { window.__clipWrites.push([text, err.name]); throw err; }) });
    const writeItems = navigator.clipboard.write.bind(navigator.clipboard);
    Object.defineProperty(navigator.clipboard, 'write', { configurable: true,
      value: (items) => writeItems(items).then(
        () => { window.__clipWrites.push(['<items>', 'ok']); },
        (err) => { window.__clipWrites.push(['<items>', err.name]); throw err; }) });
  }
})()"""

# Everything the page could have kept: web storage, every IndexedDB record,
# every cached response, and the document with its fields' values.
KEPT_JS = """async () => {
  const out = { local: {}, session: {}, idb: {}, caches: {} };
  for (let i = 0; i < localStorage.length; i++) out.local[localStorage.key(i)] = localStorage.getItem(localStorage.key(i));
  for (let i = 0; i < sessionStorage.length; i++) out.session[sessionStorage.key(i)] = sessionStorage.getItem(sessionStorage.key(i));
  if (indexedDB.databases) {
    for (const { name } of await indexedDB.databases()) {
      const db = await new Promise((res, rej) => { const r = indexedDB.open(name); r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error); });
      out.idb[name] = {};
      for (const store of db.objectStoreNames) {
        out.idb[name][store] = await new Promise((res) => {
          const r = db.transaction(store).objectStore(store).getAll();
          r.onsuccess = () => res(r.result); r.onerror = () => res(null);
        });
      }
      db.close();
    }
  }
  if (self.caches) {
    for (const key of await caches.keys()) {
      const cache = await caches.open(key);
      out.caches[key] = [];
      for (const req of await cache.keys()) {
        const resp = await cache.match(req);
        out.caches[key].push([req.url, resp ? await resp.text() : '']);
      }
    }
  }
  out.document = document.documentElement.outerHTML + '\\n'
    + [...document.querySelectorAll('textarea, input')].map((e) => e.value).join('\\n');
  out.messages = JSON.stringify(window.__clipMsgs || []);
  return JSON.stringify(out);
}"""


def wait_until(fn, timeout: float, step: float = 0.25):
    deadline = time.time() + timeout
    value = fn()
    while not value and time.time() < deadline:
        time.sleep(step)
        value = fn()
    return value


class Owner:
    """The session's clipboard owner: an X client, or a data-control source on
    the capture compositor, offering the text and the hint beside it."""

    def __init__(self, wayland: bool, text: str, hint: str = HINTS[0], secret: bool = True) -> None:
        self.wayland = wayland
        entries = {hint: b"secret"} if secret else {}
        if wayland:
            import pixelflux
            os.environ["XDG_RUNTIME_DIR"] = H.RUNTIME_DIR
            self.source = pixelflux.ScreenCapture()
            self.source.clipboard_write_app(WL_SOCKET, [("text/plain;charset=utf-8", text.encode()),
                                                        ("text/plain", text.encode()),
                                                        *entries.items()])
        else:
            _, self.stop = H.x_own_clipboard(text.encode(), entries)

    def release(self) -> None:
        """Let the selection go, as a password manager clearing its copy does."""
        if self.wayland:
            self.source.clipboard_clear_app(WL_SOCKET)
        else:
            self.stop["flag"] = True


def local_text(page) -> str:
    return page.evaluate("navigator.clipboard.readText().catch((e) => 'ERR:' + e.name)")


def kept(page) -> dict:
    return json.loads(page.evaluate(KEPT_JS))


def leaks(page, secret: str) -> list:
    """Where the page keeps `secret` or its base64: nowhere, or the places."""
    forms = (secret, base64.b64encode(secret.encode()).decode())
    return [where for where, value in kept(page).items()
            if any(form in json.dumps(value, ensure_ascii=False) for form in forms)]


def open_page(pw, engine: str, mode: str, dashboard: str):
    if engine == "firefox":
        closer = ctx = C.firefox_persistent_context(pw, viewport={"width": 1440, "height": 900})
    else:
        closer = C.launch_browser(pw, engine)
        ctx = closer.new_context(viewport={"width": 1440, "height": 900})
    perms = {"chromium": ["clipboard-read", "clipboard-write"], "firefox": ["clipboard-read"],
             "webkit": []}[engine]
    if perms:
        try:
            ctx.grant_permissions(perms, origin=H.BASE_URL)
        except Exception:
            pass
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(TAP_JS)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    # The clipboard needs the session's settings, which carry its clipboard policy.
    connected = wait_until(lambda: page.evaluate("window.clipboard_enabled === true"), 60, step=0.5)
    return closer, page, bool(connected)


def last_preview(page) -> dict:
    return page.evaluate("(window.__clipMsgs || []).slice(-1)[0] || null") or {}


def pane(page, dashboard: str) -> str:
    """What the dashboard's clipboard box shows."""
    if not TD.open_clipboard_panel(page, dashboard):
        return "<no panel>"
    area = page.locator("#dashboardClipboardTextarea").first
    try:
        area.wait_for(state="visible", timeout=4000)
        return area.input_value()
    except Exception as e:
        return f"<no textarea: {e}>"


def close_panel(page, dashboard: str) -> None:
    if dashboard == "wish":
        page.keyboard.press("Escape")
        time.sleep(0.3)


def masked_checks(res: "H.Results", tag: str, page, wayland: bool, dashboard: str,
                  engine: str, n: int) -> None:
    """The copy reaches the page masked, is copied by the button, and is kept nowhere."""
    secret = f"pw-{tag}-{n}-Zq8!"
    hint = HINTS[n % len(HINTS)]
    owner = Owner(wayland, secret, hint)
    try:
        got = wait_until(lambda: last_preview(page).get("secret") is True, 10)
        res.check(f"{tag} a copy marked {hint} reaches the page as the flag alone",
                  got and last_preview(page).get("text") == "", last_preview(page))
        shown = pane(page, dashboard)
        res.check(f"{tag} the clipboard pane shows it masked", shown == MASK, shown[:40])
        button = page.locator('button:has-text("Copy hidden text")').first
        page.evaluate("window.postMessage({type: 'settings', settings: {clipboard_seamless: false}}, "
                      "window.location.origin)")
        if engine == "chromium":
            page.evaluate("navigator.clipboard.writeText('before the button')")
        clicked = False
        if button.count():
            button.click()
            clicked = True
        # Headless WebKit reads nothing back, so there the write's own outcome decides.
        copied = wait_until(lambda: [secret, "ok"] in page.evaluate("window.__clipWrites"), 5)
        if engine != "webkit":
            copied = copied and wait_until(lambda: local_text(page) == secret, 5)
        res.check(f"{tag} the pane's button copies it to this device", clicked and copied,
                  f"clicked={clicked} writes={len(page.evaluate('window.__clipWrites'))}")
        page.evaluate("window.postMessage({type: 'settings', settings: {clipboard_seamless: true}}, "
                      "window.location.origin)")
        close_panel(page, dashboard)
        places = leaks(page, secret)
        res.check(f"{tag} the page keeps it in no storage, field, or message", not places, places)
    finally:
        owner.release()
    time.sleep(1.5)
    owner = Owner(wayland, f"ordinary-{tag}-{n}", secret=False)
    try:
        got = wait_until(lambda: last_preview(page).get("text") == f"ordinary-{tag}-{n}", 10)
        shown = pane(page, dashboard)
        res.check(f"{tag} ordinary text is shown as it is",
                  got and shown == f"ordinary-{tag}-{n}" and last_preview(page).get("secret") is False,
                  shown[:40])
        close_panel(page, dashboard)
    finally:
        owner.release()
    time.sleep(1.0)


def take_back_checks(res: "H.Results", tag: str, page, wayland: bool) -> None:
    """Chromium takes a secret back when the session lets it go, and when its
    term is up, only while the local clipboard still holds it."""
    secret = f"pw-{tag}-release-Zq8!"
    owner = Owner(wayland, secret)
    landed = wait_until(lambda: local_text(page) == secret, 10)
    res.check(f"{tag} with seamless sync the secret lands on the local clipboard", landed, local_text(page))
    started = time.time()
    owner.release()
    taken = wait_until(lambda: local_text(page) == "", 8)
    res.check(f"{tag} the session letting it go takes it off the local clipboard",
              taken, f"{local_text(page)!r} after {time.time() - started:.1f} s")

    secret = f"pw-{tag}-kept-Zq8!"
    owner = Owner(wayland, secret)
    wait_until(lambda: local_text(page) == secret, 10)
    page.evaluate("navigator.clipboard.writeText('a copy the user made')")
    owner.release()
    time.sleep(4.0)
    res.check(f"{tag} a copy the user made since is left alone when the session lets go",
              local_text(page) == "a copy the user made", local_text(page))

    secret = f"pw-{tag}-term-Zq8!"
    owner = Owner(wayland, secret)
    try:
        wait_until(lambda: local_text(page) == secret, 10)
        landed_at = time.time()
        time.sleep(TERM_S - 5)
        early = local_text(page)
        taken = wait_until(lambda: local_text(page) == "", 12, step=0.5)
        res.check(f"{tag} a secret still held is taken back when its term is up, not before",
                  early == secret and taken,
                  f"at {TERM_S - 5:.0f} s {early[:20]!r}, emptied after {time.time() - landed_at:.1f} s")
    finally:
        owner.release()
    time.sleep(1.5)

    secret = f"pw-{tag}-term-kept-Zq8!"
    owner = Owner(wayland, secret)
    try:
        wait_until(lambda: local_text(page) == secret, 10)
        page.evaluate("navigator.clipboard.writeText('another copy the user made')")
        time.sleep(TERM_S + 5)
        res.check(f"{tag} a copy the user made since is left alone when the term is up",
                  local_text(page) == "another copy the user made", local_text(page))
    finally:
        owner.release()
    time.sleep(1.0)


def viewer_checks(res: "H.Results", tag: str, browser, mode: str, wayland: bool) -> None:
    """A shared viewer's page, beside the controller's, takes nothing of the
    session's clipboard."""
    ctx = browser.new_context(viewport={"width": 1280, "height": 720})
    try:
        ctx.grant_permissions(["clipboard-read", "clipboard-write"], origin=H.BASE_URL)
        ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
        ctx.add_init_script(TAP_JS)
        viewer = ctx.new_page()
        viewer.goto(H.BASE_URL + "/#shared", wait_until="load")
        time.sleep(10.0)
        seen = viewer.evaluate("window.__clipMsgs.length")
        for n, secret in enumerate((True, False)):
            owner = Owner(wayland, f"{'pw' if secret else 'text'}-{tag}-viewer-{n}-Zq8!", secret=secret)
            time.sleep(3.0)
            owner.release()
            time.sleep(1.5)
        got = viewer.evaluate("window.__clipMsgs.slice(%d)" % seen)
        writes = viewer.evaluate("window.__clipWrites.length")
        res.check(f"{tag} a shared viewer's page takes nothing of the session's clipboard, a secret or not",
                  not got and not writes, f"previews={got} writes={writes}")
    finally:
        ctx.close()


def block(mode: str, wayland: bool) -> "H.Results":
    res = H.Results(f"clipsecret-{'wl' if wayland else mode}")
    for dashboard, dist in (("classic", H.CLASSIC_DIST), ("wish", H.WISH_DIST)):
        tag = f"[{'wl' if wayland else mode}/{dashboard}]"
        H.server_start(mode=mode, wayland=wayland, web_root=dist)
        with sync_playwright() as p:
            closer, page, connected = open_page(p, "chromium", mode, dashboard)
            try:
                res.check(f"{tag} the session's clipboard policy arrived", connected)
                masked_checks(res, tag, page, wayland, dashboard, "chromium",
                              1 if dashboard == "classic" else 2)
                if dashboard == "classic":
                    take_back_checks(res, tag, page, wayland)
                    viewer_checks(res, tag, closer, mode, wayland)
            finally:
                closer.close()
        H.server_stop()
    return res


def engines_block() -> "H.Results":
    """Firefox and WebKit: masked and kept nowhere; nothing read to take it back."""
    res = H.Results("clipsecret-engines")
    H.server_start(mode="websockets", wayland=False, web_root=H.CLASSIC_DIST)
    for engine in ("firefox", "webkit"):
        tag = f"[websockets/{engine}]"
        with sync_playwright() as p:
            try:
                closer, page, connected = open_page(p, engine, "websockets", "classic")
            except Exception as e:
                res.skip(f"{tag} engine unavailable", str(e)[:120])
                continue
            try:
                res.check(f"{tag} the session's clipboard policy arrived", connected)
                masked_checks(res, tag, page, False, "classic", engine, 3 if engine == "firefox" else 4)
                reads = page.evaluate("window.__clipReads")
                owner = Owner(False, f"pw-{engine}-limit-Zq8!")
                wait_until(lambda page=page: last_preview(page).get("secret") is True, 10)
                owner.release()
                time.sleep(4.0)
                res.check(f"{tag} nothing is read to take a secret back, which would raise a paste prompt",
                          page.evaluate("window.__clipReads") == reads,
                          f"{page.evaluate('window.__clipReads') - reads} reads")
            finally:
                closer.close()
    H.server_stop()
    return res


def main() -> int:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    results = []
    if which in ("websockets", "all"):
        results.append(block("websockets", False))
    if which in ("webrtc", "all"):
        results.append(block("webrtc", False))
    if which in ("wayland", "all"):
        results.append(block("websockets", True))
    if which in ("engines", "all"):
        results.append(engines_block())
    ok = all(r.summary() for r in results)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
