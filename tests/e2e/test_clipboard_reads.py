#!/usr/bin/env python3
"""Which engines the client reads the local clipboard on, and when.

Firefox and WebKit raise a paste prompt on every `navigator.clipboard.read()`
or `readText()` outside a paste gesture, and Firefox's prompt swallows the
next clicks and blocks tools that type into the page; the only silent read
there is the `paste` event's own data. Chromium reads on focus once the
permission is granted. The async read API is counted on each engine across
the moments the client syncs the clipboard: load, focus, a Ctrl+V chord with
nothing on the clipboard, and one with text on it, which is a real paste. With the client-to-session direction turned off by the
server (`enable_clipboard=out`), nothing reads on any engine.
Usage: python3 tests/e2e/test_clipboard_reads.py
"""
import os
import sys
import time
from typing import Any, List

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

# Counts every entry into the async read API, whoever calls it.
INSTRUMENT_JS = """(() => {
  window.__clipReads = [];
  const wrap = (name) => {
    if (!navigator.clipboard || typeof navigator.clipboard[name] !== 'function') return;
    const orig = navigator.clipboard[name].bind(navigator.clipboard);
    Object.defineProperty(navigator.clipboard, name, {
      configurable: true,
      value: function(...args) {
        window.__clipReads.push(name);
        return orig(...args);
      }
    });
  };
  wrap('read');
  wrap('readText');
})()"""

ENGINES = ("firefox", "chromium", "webkit")


def reads(page: Any) -> List[str]:
    return page.evaluate("window.__clipReads || []")


def open_page(pw: Any, engine: str, mode: str) -> Any:
    """Returns what to close and the page; Firefox runs on the persistent profile."""
    if engine == "firefox":
        browser = None
        ctx = C.firefox_persistent_context(pw, viewport={"width": 1280, "height": 720})
    else:
        browser = C.launch_browser(pw, engine)
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
    try:
        perms = {"chromium": ["clipboard-read", "clipboard-write"],
                 "firefox": ["clipboard-read"], "webkit": []}[engine]
        if perms:
            ctx.grant_permissions(perms, origin=H.BASE_URL)
    except Exception:
        pass
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(C.WIRE_TAP_JS)
    ctx.add_init_script(INSTRUMENT_JS)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    return browser or ctx, page


def provoke(page: Any) -> dict:
    """The moments a sync could read on, each followed by a settle."""
    counts = {}
    counts["load"] = len(reads(page))
    page.evaluate("window.dispatchEvent(new Event('blur'))")
    page.evaluate("window.dispatchEvent(new Event('focus'))")
    page.evaluate("window.dispatchEvent(new Event('focus'))")
    time.sleep(1.0)
    counts["focus"] = len(reads(page)) - counts["load"]
    page.evaluate("document.getElementById('overlayInput').focus()")
    page.keyboard.press("Control+v")
    time.sleep(1.0)
    counts["chord"] = len(reads(page)) - counts["load"] - counts["focus"]
    # A real paste: the text is put on the clipboard with a write, which
    # raises no prompt, and the chord makes the browser fire the paste event
    # with that data on it.
    page.evaluate("navigator.clipboard.writeText('pasted text').catch(() => {})")
    time.sleep(0.5)
    page.evaluate("document.getElementById('overlayInput').focus()")
    page.keyboard.press("Control+v")
    time.sleep(1.5)
    counts["paste"] = len(reads(page)) - sum(counts.values())
    return counts


def drive(res: "H.Results", mode: str, engine: str, clipboard_in: bool) -> None:
    extra = {} if clipboard_in else {"SELKIES_ENABLE_CLIPBOARD": "out"}
    tag = f"[{mode}/{engine}{'' if clipboard_in else ', clipboard in off'}]"
    if engine == "firefox" and mode == "webrtc" and not C.openh264_version():
        res.skip(f"{tag} no OpenH264 GMP plugin in {C.FF_E2E_PROFILE}",
                 "run tests/tools/fetch-openh264.sh to cover H.264 in Firefox")
        return
    H.server_start(mode=mode, wayland=False, extra_env=extra)
    with sync_playwright() as p:
        try:
            closer, page = open_page(p, engine, mode)
        except Exception as e:
            res.skip(f"{tag} engine unavailable", str(e)[:120])
            return
        try:
            video = (C.wait_wr_video(page, timeout=90) if mode == "webrtc"
                     else C.wait_ws_video(page, timeout=60))
            res.check(f"{tag} video flows", bool(video), "")
            counts = provoke(page)
            prompts = engine != "chromium"
            if not clipboard_in:
                res.check(f"{tag} nothing reads the clipboard", sum(counts.values()) == 0, counts)
            elif prompts:
                res.check(f"{tag} the async read API is never entered", sum(counts.values()) == 0, counts)
            else:
                res.check(f"{tag} a focus reads once the permission is granted",
                          counts["focus"] >= 1, counts)
                res.check(f"{tag} a Ctrl+V chord and a paste event read nothing themselves",
                          counts["chord"] == 0 and counts["paste"] == 0, counts)
            if prompts and clipboard_in:
                sent = page.evaluate("(window.__wireSent || []).filter(d => typeof d === 'string' "
                                     "&& d.startsWith('cw')).length")
                policy = page.evaluate("[window.clipboard_enabled, document.activeElement && document.activeElement.id]")
                res.check(f"{tag} a paste event still reaches the session", sent > 0,
                          f"sent={sent} policy/active={policy}")
        finally:
            closer.close()


def main() -> "H.Results":
    res = H.Results("clipboard-reads")
    xproc = None
    if not H.TEST_DISPLAY:
        xproc, xdisp = H.private_x_server()
        H.TEST_DISPLAY = xdisp
    try:
        for engine in ENGINES:
            for mode in ("websockets", "webrtc"):
                drive(res, mode, engine, True)
        drive(res, "websockets", "chromium", False)
    finally:
        H.server_stop()
        if xproc is not None:
            H.stop_x_server(xproc, H.TEST_DISPLAY)
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
