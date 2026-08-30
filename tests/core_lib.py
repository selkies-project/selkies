#!/usr/bin/env python3
"""Shared check routines for the transport x backend e2e matrix."""
import json
import os
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

BROWSER_ARGS: list = [
    "--no-sandbox", "--use-gl=swiftshader", "--mute-audio",
    "--autoplay-policy=no-user-gesture-required",
]
# Playwright's bundled Chromium unless a system browser is named, which is how a
# release Chrome build gets covered (its media stack differs from Chromium's)
CHROME_PATH: Optional[str] = os.environ.get("E2E_CHROME") or None
# System Firefox likewise: Playwright's bundled Firefox build lacks H264 (no
# OpenH264), so H264 streams can never play there.
FIREFOX_PATH: Optional[str] = os.environ.get("E2E_FIREFOX") or None


# The socket lives in a worker on the websockets transport, so wrapping
# `WebSocket.prototype.send` on the page sees nothing of the wire. Every
# page-side send goes through the transport handle the core publishes as
# `window.selkiesTransport`, whichever thread owns the socket, so a hook is
# installed on that handle as well as on the prototypes (the WebRTC data
# channel, and a socket the page still owns). An existing accessor is chained
# rather than replaced, so this composes with `launch_client`'s receive tap.
def wire_hook_js(body: str) -> str:
    """An init script running `body` on every page-side send, with the payload
    in `data`; mutating an ArrayBuffer in place changes what goes out.

    Args:
        body: JavaScript statements, evaluated with `data` in scope.

    Returns:
        The script, for `context.add_init_script`.
    """
    return """
(() => {
  const hook = (data) => { %s };
  for (const proto of [window.RTCDataChannel && RTCDataChannel.prototype,
                       window.WebSocket && WebSocket.prototype]) {
    if (!proto || typeof proto.send !== 'function') continue;
    const orig = proto.send;
    proto.send = function (d) { try { hook(d); } catch (e) {} return orig.call(this, d); };
  }
  const wrap = (t) => {
    if (!t || t.__wireHooked || typeof t.send !== 'function') return t;
    t.__wireHooked = true;
    const orig = t.send.bind(t);
    t.send = (d) => { try { hook(d); } catch (e) {} return orig(d); };
    return t;
  };
  const prev = Object.getOwnPropertyDescriptor(window, 'selkiesTransport');
  let held = null;
  Object.defineProperty(window, 'selkiesTransport', {
    configurable: true,
    get: () => (prev && prev.get ? prev.get() : held),
    set: (v) => { const w = wrap(v); held = w; if (prev && prev.set) prev.set(w); },
  });
})();
""" % body


# Text messages the page sent, in `window.__wireSent`. Only strings are kept:
# a binary payload is transferred to the socket worker and detached, so a
# reference held here would read as empty.
WIRE_TAP_JS = ("window.__wireSent = [];\n" + wire_hook_js(
    "if (typeof data === 'string') window.__wireSent.push(data);"))


def chromium_launch(pw: Any) -> Any:
    """Launch headless Chromium (or the system Chrome named by E2E_CHROME)."""
    kwargs = {"headless": True, "args": BROWSER_ARGS}
    if CHROME_PATH:
        kwargs["executable_path"] = CHROME_PATH
    return pw.chromium.launch(**kwargs)


def launch_browser(pw: Any, engine: str = "chromium") -> Any:
    """Launch a headless browser for ``engine``: chromium, firefox, or webkit."""
    if engine == "firefox":
        # The <video> carries the audio stream, so Firefox's default autoplay
        # policy rejects the initial play() without a user gesture; the same
        # allowance Chromium gets through --autoplay-policy (BROWSER_ARGS).
        kwargs = {"headless": True, "firefox_user_prefs": {
            "media.autoplay.default": 0,
            "media.autoplay.blocking_policy": 0,
            "media.autoplay.block-webaudio": False,
        }}
        if FIREFOX_PATH:
            kwargs["executable_path"] = FIREFOX_PATH
        return pw.firefox.launch(**kwargs)
    if engine == "webkit":
        return pw.webkit.launch(headless=True)
    return chromium_launch(pw)


def launch_chrome(pw: Any, url_hash: str = "", mode: Optional[str] = None,
                  engine: str = "chromium") -> tuple:
    """Launch an instrumented browser page on the test server.

    The init script counts WebSocket binary frames because headless rAF
    throttling makes the client's own fps counter read 0 even while the stream
    flows, and records clipboard/display/role postMessages for the checks.

    Args:
        pw: The sync_playwright handle.
        url_hash: Fragment appended to the base URL (e.g. `#display2`).
        mode: Authoritative transport at load time; skips the
            localStorage-default mode-flip probe/reload (the hook dashboards
            use after /api/status).
        engine: Browser engine to drive (chromium, firefox, or webkit).

    Returns:
        `(browser, page, console_errors, not_found)`; the two lists keep
        filling as the page runs.
    """
    browser = launch_browser(pw, engine)
    ctx = browser.new_context(
        permissions=[],
        viewport={"width": 1280, "height": 720},
        device_scale_factor=1,
    )
    try:
        perms = {"chromium": ["clipboard-read", "clipboard-write"],
                 "firefox": ["clipboard-read"],
                 "webkit": []}[engine]
        if perms:
            ctx.grant_permissions(perms, origin=H.BASE_URL)
    except Exception:
        pass
    if mode:
        ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    page = ctx.new_page()
    console_errors = []
    page.on("console", lambda m: console_errors.append(m.text) if m.type == "error" else None)
    page.on("pageerror", lambda e: console_errors.append(str(e)))
    not_found = []
    page.on("response", lambda r: not_found.append(r.url) if r.status == 404 else None)
    page.add_init_script("""
      // Instrument WebSocket binary frames: headless rAF throttling makes the
      // client's fps counter read 0 even while the stream flows.
      window.__wsFrames = 0;
      (() => {
        const tap = (e) => {
          if (e.data instanceof ArrayBuffer) window.__wsFrames++;
          else if (typeof e.data === 'string') {
            window.__wsTexts = window.__wsTexts || [];
            if (e.data.includes('DISPLAY_CONFIG_UPDATE')) window.__wsTexts.push(e.data);
          }
        };
        const WS = window.WebSocket;
        window.WebSocket = function(...a) {
          const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
          s.__rxTapped = true;
          s.addEventListener('message', tap);
          return s;
        };
        window.WebSocket.prototype = WS.prototype;
        Object.setPrototypeOf(window.WebSocket, WS);
        // The websockets transport runs its socket in a worker; its receive
        // side is observed through the page handle, which never passes above.
        let transport = null;
        Object.defineProperty(window, 'selkiesTransport', {
          configurable: true,
          get: () => transport,
          set: (v) => {
            transport = v;
            if (v && v.addEventListener && !v.__rxTapped) {
              v.__rxTapped = true;
              v.addEventListener('message', tap);
            }
          },
        });
      })();
      window.__clipMsgs = [];
      window.__displayCfg = [];
      window.__roleUpdates = [];
      window.addEventListener('message', (e) => {
        if (!e.data || !e.data.type) return;
        if (e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
        if (e.data.type === 'displayConfigUpdate' || e.data.type === 'DISPLAY_CONFIG_UPDATE') window.__displayCfg.push(e.data);
        if (e.data.type === 'clientRoleUpdate') window.__roleUpdates.push(e.data.role);
      });
    """)
    page.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return browser, page, console_errors, not_found


def new_page(context: Any, mode: Optional[str] = None, url_hash: str = "") -> Any:
    """A second/third window with the same init instrumentation (display2,
    shared/viewer roles)."""
    if mode:
        context.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    pg = context.new_page()
    pg.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return pg


def benign_console(console_errors: list, not_found: list) -> tuple:
    """Split observed console errors and 404s into real ones and known noise.

    The 409 handshake error is the client's own mode-flip probe converging to
    the server's authoritative transport when localStorage runs stale;
    manifest/favicon 404s are the browser's own probing.

    Returns:
        `(real_errors, bad404)` — anything listed here fails the check.
    """
    benign_pats = ("Failed to load resource", "Unexpected server response:")
    real_errors = [e for e in console_errors if not any(bp in e for bp in benign_pats)]
    benign = [u for u in not_found if u.endswith("/manifest.json") or "favicon" in u]
    bad404 = [u for u in not_found if u not in benign]
    return real_errors, bad404


def wait_ws_video(page: Any, timeout: float = 15) -> Optional[dict]:
    """Wait for WebSocket video: a stream-sized canvas plus a received chunk.

    A stream-sized canvas alone is not proof of video: the client sizes it
    from layout messages before any frame arrives, and a capture that failed to
    start never sends one. Require a received video chunk as well, mirroring
    the decoded-frame requirement wait_wr_video gets from videoWidth.

    Returns:
        The canvas dimensions as a dict, or None on timeout.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        info = page.evaluate("""(() => {
          if (!(window.videoChunksReceived > 0)) return null;
          const cs = document.querySelectorAll('canvas');
          for (const c of cs) { if (c.width >= 640) return {w: c.width, h: c.height}; }
          return null;
        })()""")
        if info:
            return info
        time.sleep(0.5)
    return None


def wait_wr_video(page: Any, timeout: float = 45) -> Optional[dict]:
    """Wait for a decoding WebRTC `<video>`; its dimensions and audio track
    count as a dict, or None on timeout."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        info = page.evaluate("""(() => {
          const v = document.querySelector('video');
          if (v && v.readyState >= 2 && v.videoWidth > 0) {
            return {w: v.videoWidth, h: v.videoHeight,
                    audio: v.srcObject && v.srcObject.getAudioTracks ? v.srcObject.getAudioTracks().length : -1};
          }
          return null;
        })()""")
        if info:
            return info
        time.sleep(0.5)
    return None


def page_fps(page: Any, timeout: float = 10) -> float:
    """First non-zero client fps reading, or 0 after `timeout` seconds."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        fps = page.evaluate("window.fps")
        if fps and fps > 0:
            return fps
        time.sleep(0.5)
    return 0


def server_clipboard_push_check(page: Any, payload_text: str) -> bool:
    """Whether the page received `payload_text` as a server clipboard push."""
    got = page.evaluate("window.__clipMsgs.map(m => m.text)")
    return payload_text in got


def send_clipboard_from_client(page: Any, text: str, engine: str = "chromium") -> None:
    """Drive client->server clipboard truthfully per engine.

    Chromium reads the clipboard on window focus. Firefox/WebKit only forward
    through the 'paste' event — and Firefox *strips* clipboardData from
    synthetic ClipboardEvents, so there we write the clipboard for real and
    chords a real Ctrl+V (Playwright key events are trusted, so the browser
    fires a genuine paste that the core's paste listener consumes)."""
    page.evaluate(f"navigator.clipboard.writeText({json.dumps(text)}).catch(() => {{}})")
    time.sleep(0.4)
    if engine == "chromium":
        # The read the focus handler performs needs the document to actually be
        # focused; a synthetic Event('focus') runs the handler but leaves
        # document.hasFocus() false, and the read then rejects.
        page.bring_to_front()
        page.evaluate("window.dispatchEvent(new Event('focus'))")
    else:
        # The honest path on WebKit; Firefox strips clipboardData, so there
        # this dispatch is a no-op and the Ctrl+V chord below does the work.
        page.evaluate("""(text) => {
          const dt = new DataTransfer();
          dt.setData('text/plain', text);
          const ev = new ClipboardEvent('paste', {clipboardData: dt, bubbles: true, cancelable: true});
          window.dispatchEvent(ev);
        }""", text)
        if engine == "webkit":
            return
        # Firefox: the paste-ordering hold may consume the first Ctrl+V (it
        # replays the chord synthetically, with no native paste); the second
        # press, with the clipboard re-written first, performs the real paste.
        for _ in range(2):
            page.keyboard.down("Control")
            page.keyboard.press("v")
            page.keyboard.up("Control")
            time.sleep(0.7)
            page.evaluate(f"navigator.clipboard.writeText({json.dumps(text)}).catch(() => {{}})")
            time.sleep(0.3)
    time.sleep(0.3)


def settings_change(page: Any, settings: dict) -> None:
    """Post a settings update into the page as the dashboards do."""
    page.evaluate(
        "(s) => window.postMessage({type: 'settings', settings: s}, window.location.origin)",
        settings)


def wait_log(substr: str, timeout: float = 12, log: str = H.LOG) -> bool:
    """Wait for `substr` to appear in the server log, biased to recent output."""
    deadline = time.time() + timeout
    last_len = len(H.server_log(log))
    while time.time() < deadline:
        txt = H.server_log(log)
        i = txt.find(substr, max(0, last_len - 500000))
        if i >= 0:
            return True
        time.sleep(0.5)
    return H.server_log(log).find(substr) >= 0


def wait_log_absent(substr: str, timeout: float = 6, log: str = H.LOG) -> bool:
    """True when `substr` still has not appeared after `timeout` seconds."""
    time.sleep(timeout)
    return substr not in H.server_log(log)


def x11_keymap_pressed(keysym_char: str = "x") -> Optional[bool]:
    """Whether the X server currently reports the key for `keysym_char` as
    held, straight from query_keymap; None if the keysym has no keycode."""
    from selkies.Xlib import XK
    code = None
    d = H.x_display()
    try:
        for name in ("XK_" + keysym_char,):
            ks = getattr(XK, name, None)
            if ks is not None:
                code = d.keysym_to_keycode(ks)
                break
        if not code:
            return None
        keymap = d.query_keymap()
        byte = keymap[code // 8]
        return bool(byte & (1 << (code % 8)))
    finally:
        d.close()


def x11_mouse_pos() -> tuple:
    """Pointer position straight from the X server, so no xdotool is needed."""
    d = H.x_display()
    try:
        p = d.screen().root.query_pointer()
        return p.root_x, p.root_y
    finally:
        d.close()


def x11_buttons_held() -> tuple:
    """Core pointer buttons the X server reports held, lowest first.

    The reply's modifier mask carries Button1Mask at bit 8, so a held drag is
    visible without asking the client what it thinks it sent.
    """
    d = H.x_display()
    try:
        mask = d.screen().root.query_pointer().mask
        return tuple(b for b in range(1, 6) if mask & (1 << (7 + b)))
    finally:
        d.close()
