#!/usr/bin/env python3
"""Formatted content survives the trip, and the switches over it hold.

A copy carrying markup travels with the plain text its source wrote, in both
directions, so a rich editor on either side keeps the styling and a plain
field gets the text. The panel's switches are then driven from the server:
with seamless sync off nothing moves on its own, and with the client's chords
handed over, Control+Shift+F reaches the session instead of the browser.
Usage: python3 tests/e2e/test_clipboard_rich.py [websockets|webrtc|policy|all]
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

sys.path.insert(0, os.path.join(H.REPO, "src"))
from selkies.Xlib import X, display as xdisp  # noqa: E402
from selkies.input_handler import _X11ClipboardMonitor  # noqa: E402

DASH = os.path.join(H.REPO, "addons/selkies-dashboard/dist")
HTML, PLAIN = "<b>rich copy</b>", "rich copy"
BACK_HTML, BACK_PLAIN = "<i>from the session</i>", "from the session"


def convert(target: str):
    """One selection conversion on a connection of its own, as a paste does."""
    d = xdisp.Display(H.require_display())
    try:
        screen = d.screen()
        win = screen.root.create_window(0, 0, 1, 1, 0, screen.root_depth,
                                        window_class=X.InputOutput)
        prop = d.get_atom("SELKIES_PASTE")
        win.convert_selection(d.get_atom("CLIPBOARD"), d.get_atom(target), prop, X.CurrentTime)
        d.flush()
        deadline = time.time() + 5
        while time.time() < deadline:
            if d.pending_events():
                event = d.next_event()
                if event.type == X.SelectionNotify:
                    if event.property == X.NONE:
                        return None
                    value = win.get_full_property(prop, X.AnyPropertyType)
                    return None if value is None else bytes(value.value)
            time.sleep(0.01)
        return None
    finally:
        d.close()


def page_with_clipboard(p, mode: str):
    """A Chromium page on the session, allowed to read and write the clipboard."""
    browser = p.chromium.launch(args=["--no-sandbox"])
    ctx = browser.new_context(viewport={"width": 1440, "height": 900})
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    try:
        ctx.grant_permissions(["clipboard-read", "clipboard-write"], origin=H.BASE_URL)
    except Exception:
        pass
    page = ctx.new_page()
    page.goto(H.BASE_URL, wait_until="load")
    time.sleep(12.0)
    return browser, page


def write_rich(page, html: str, text: str) -> None:
    page.evaluate("""async ([html, text]) => {
      await navigator.clipboard.write([new ClipboardItem({
        'text/html': new Blob([html], {type: 'text/html'}),
        'text/plain': new Blob([text], {type: 'text/plain'}),
      })]);
    }""", [html, text])
    time.sleep(0.6)
    page.bring_to_front()
    page.evaluate("window.dispatchEvent(new Event('focus'))")
    time.sleep(4.0)


def read_rich(page):
    return page.evaluate("""async () => {
      try {
        const items = await navigator.clipboard.read();
        for (const item of items) {
          if (!item.types.includes('text/html')) continue;
          return { html: await (await item.getType('text/html')).text(),
                   text: item.types.includes('text/plain')
                     ? await (await item.getType('text/plain')).text() : null };
        }
        return null;
      } catch (err) { return 'read failed: ' + err.name; }
    }""")


def block(mode: str) -> "H.Results":
    res = H.Results(f"cliprich-{mode}")
    H.server_start(mode=mode, wayland=False, web_root=DASH,
                   extra_env={"PYTHONPATH": os.path.join(H.REPO, "src")})
    with sync_playwright() as p:
        browser, page = page_with_clipboard(p, mode)
        source = None
        try:
            write_rich(page, HTML, PLAIN)
            html = convert("text/html")
            plain = convert("UTF8_STRING")
            res.check("a rich copy reaches the session as markup",
                      html is not None and html.decode() == HTML, html)
            res.check("its plain text reaches the session beside the markup",
                      plain is not None and plain.decode() == PLAIN, plain)

            source = _X11ClipboardMonitor(H.require_display())
            source.offer([("text/html", BACK_HTML.encode()), ("text/plain", BACK_PLAIN.encode())])
            # Two gestures: the write needs a user activation, and the payload
            # has to have arrived before the one that lands it.
            for _ in range(2):
                page.mouse.move(300, 300)
                page.mouse.down()
                page.mouse.up()
                time.sleep(2.5)
            local = read_rich(page)
            res.check("a rich copy out of the session reaches the browser as markup",
                      isinstance(local, dict) and local.get("html") == BACK_HTML, local)
            res.check("its plain text reaches the browser beside the markup",
                      isinstance(local, dict) and local.get("text") == BACK_PLAIN, local)
        finally:
            source.close() if source is not None else None
            browser.close()
    res.summary()
    return res


def chord(page) -> tuple:
    """Hold Control+Shift+F, and report what took it: the browser, the session."""
    page.mouse.move(300, 300)
    page.mouse.down()
    page.mouse.up()
    time.sleep(1.0)
    page.keyboard.down("Control")
    page.keyboard.down("Shift")
    page.keyboard.down("f")
    time.sleep(1.5)
    held = C.x11_keymap_pressed("f")
    fullscreen = page.evaluate("document.fullscreenElement !== null")
    for key in ("f", "Shift", "Control"):
        page.keyboard.up(key)
    return fullscreen, held


def policy_block() -> "H.Results":
    res = H.Results("cliprich-policy")
    sentinel = "nothing moved on its own"
    H.server_start(mode="websockets", wayland=False, web_root=DASH,
                   extra_env={"PYTHONPATH": os.path.join(H.REPO, "src"),
                              "SELKIES_CLIPBOARD_SEAMLESS": "false"})
    source = _X11ClipboardMonitor(H.require_display())
    source.offer([("text/plain", sentinel.encode())])
    with sync_playwright() as p:
        browser, page = page_with_clipboard(p, "websockets")
        try:
            write_rich(page, "<b>should stay here</b>", "should stay here")
            plain = convert("UTF8_STRING")
            res.check("with seamless sync off a browser copy leaves the session clipboard alone",
                      plain is not None and plain.decode() == sentinel, plain)
            local = read_rich(page)
            res.check("and the session's own copy is not written to the browser either",
                      not (isinstance(local, dict) and local.get("text") == sentinel), local)
        finally:
            source.close()
            browser.close()

    # Reading markup out of the browser is the same clipboard read images need,
    # so the image switch decides whether a rich copy leaves the browser as one.
    H.server_start(mode="websockets", wayland=False, web_root=DASH,
                   extra_env={"PYTHONPATH": os.path.join(H.REPO, "src"),
                              "SELKIES_ENABLE_BINARY_CLIPBOARD": "false"})
    with sync_playwright() as p:
        browser, page = page_with_clipboard(p, "websockets")
        try:
            write_rich(page, "<b>plain only</b>", "plain only")
            res.check("with the image clipboard off a rich copy still reaches the session as text",
                      (convert("UTF8_STRING") or b"").decode() == "plain only", convert("UTF8_STRING"))
            res.check("and it carries no markup", convert("text/html") is None, convert("text/html"))
        finally:
            browser.close()

    # The panel writes the choice and the core stores it; a reload has to find it.
    H.server_start(mode="websockets", wayland=False, web_root=DASH,
                   extra_env={"PYTHONPATH": os.path.join(H.REPO, "src")})
    with sync_playwright() as p:
        browser, page = page_with_clipboard(p, "websockets")
        try:
            page.evaluate("""window.postMessage(
                { type: 'settings', settings: { keyboard_shortcuts: false } },
                window.location.origin)""")
            time.sleep(1.5)
            page.reload(wait_until="load")
            time.sleep(12.0)
            fullscreen, held = chord(page)
            res.check("a chord choice made in the panel survives a reload",
                      fullscreen is False and held is True,
                      f"fullscreen={fullscreen} held in the session={held}")
        finally:
            browser.close()

    for shortcuts, takes_browser in (("true", True), ("false", False)):
        H.server_start(mode="websockets", wayland=False, web_root=DASH,
                       extra_env={"PYTHONPATH": os.path.join(H.REPO, "src"),
                                  "SELKIES_KEYBOARD_SHORTCUTS": shortcuts})
        with sync_playwright() as p:
            browser, page = page_with_clipboard(p, "websockets")
            try:
                fullscreen, held = chord(page)
                where = "the browser" if shortcuts == "true" else "the session"
                res.check(f"with the chords kept {shortcuts}, Control+Shift+F goes to {where}",
                          fullscreen is takes_browser and held is not takes_browser,
                          f"fullscreen={fullscreen} held in the session={held}")
            finally:
                browser.close()
    res.summary()
    return res


def main() -> None:
    selection = sys.argv[1] if len(sys.argv) > 1 else "all"
    results = []
    try:
        if selection in ("all", "websockets"):
            results.append(block("websockets"))
        if selection in ("all", "webrtc"):
            results.append(block("webrtc"))
        if selection in ("all", "policy"):
            results.append(policy_block())
    finally:
        H.server_stop()
    sys.exit(1 if any(r.failed() for r in results) else 0)


if __name__ == "__main__":
    main()
