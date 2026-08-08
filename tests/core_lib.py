#!/usr/bin/env python3
"""Shared check routines for the transport x backend e2e matrix."""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

BROWSER_ARGS = [
    "--no-sandbox", "--use-gl=swiftshader", "--mute-audio",
    "--autoplay-policy=no-user-gesture-required",
]
# Playwright's bundled Chromium unless a system browser is named, which is how a
# release Chrome build gets covered (its media stack differs from Chromium's)
CHROME_PATH = os.environ.get("E2E_CHROME") or None


def chromium_launch(pw):
    kwargs = {"headless": True, "args": BROWSER_ARGS}
    if CHROME_PATH:
        kwargs["executable_path"] = CHROME_PATH
    return pw.chromium.launch(**kwargs)


def launch_chrome(pw, url_hash="", mode=None):
    browser = chromium_launch(pw)
    ctx = browser.new_context(
        permissions=[],
        viewport={"width": 1280, "height": 720},
        device_scale_factor=1,
    )
    try:
        ctx.grant_permissions(["clipboard-read", "clipboard-write"], origin=H.BASE_URL)
    except Exception:
        pass
    if mode:
        # Authoritative transport at load time: skips the localStorage-default
        # mode-flip probe/reload (the hook dashboards use after /api/status).
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
        const WS = window.WebSocket;
        window.WebSocket = function(...a) {
          const s = a.length === 1 ? new WS(a[0]) : new WS(a[0], a[1]);
          s.addEventListener('message', (e) => {
            if (e.data instanceof ArrayBuffer) window.__wsFrames++;
            else if (typeof e.data === 'string') {
              window.__wsTexts = window.__wsTexts || [];
              if (e.data.includes('DISPLAY_CONFIG_UPDATE')) window.__wsTexts.push(e.data);
            }
          });
          return s;
        };
        window.WebSocket.prototype = WS.prototype;
        Object.setPrototypeOf(window.WebSocket, WS);
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


def new_page(context, mode=None, url_hash=""):
    """A second/third window with the same init instrumentation (display2,
    shared/viewer roles)."""
    if mode:
        context.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    pg = context.new_page()
    pg.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return pg


def benign_console(console_errors, not_found):
    # The 409 handshake error is the client's own mode-flip probe converging to
    # the server's authoritative transport when localStorage runs stale.
    benign_pats = ("Failed to load resource", "Unexpected server response:")
    real_errors = [e for e in console_errors if not any(bp in e for bp in benign_pats)]
    benign = [u for u in not_found if u.endswith("/manifest.json") or "favicon" in u]
    bad404 = [u for u in not_found if u not in benign]
    return real_errors, bad404


def wait_ws_video(page, timeout=15):
    deadline = time.time() + timeout
    while time.time() < deadline:
        info = page.evaluate("""(() => {
          const cs = document.querySelectorAll('canvas');
          for (const c of cs) { if (c.width >= 640) return {w: c.width, h: c.height}; }
          return null;
        })()""")
        if info:
            return info
        time.sleep(0.5)
    return None


def wait_wr_video(page, timeout=45):
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


def page_fps(page, timeout=10):
    deadline = time.time() + timeout
    while time.time() < deadline:
        fps = page.evaluate("window.fps")
        if fps and fps > 0:
            return fps
        time.sleep(0.5)
    return 0


def server_clipboard_push_check(page, payload_text):
    got = page.evaluate("window.__clipMsgs.map(m => m.text)")
    return payload_text in got


def send_clipboard_from_client(page, text, engine="chromium"):
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
        page.evaluate("""(text) => {
          const dt = new DataTransfer();
          dt.setData('text/plain', text);
          const ev = new ClipboardEvent('paste', {clipboardData: dt, bubbles: true, cancelable: true});
          window.dispatchEvent(ev);
        }""", text)  # honest path on WebKit; stripped to a no-op on Firefox
        if engine == "webkit":
            return
        # Firefox: the first Ctrl+V may be consumed by the paste-ordering hold
        # (it flushes any stashed server->client clipboard write and replays
        # the chord synthetically — no native paste action). A genuine second
        # press, with the clipboard re-written first, performs the real paste
        # that the core's paste listener forwards to the server.
        for _ in range(2):
            page.keyboard.down("Control")
            page.keyboard.press("v")
            page.keyboard.up("Control")
            time.sleep(0.7)
            page.evaluate(f"navigator.clipboard.writeText({json.dumps(text)}).catch(() => {{}})")
            time.sleep(0.3)
    time.sleep(0.3)


def settings_change(page, settings):
    page.evaluate(
        "(s) => window.postMessage({type: 'settings', settings: s}, window.location.origin)",
        settings)


def wait_log(substr, timeout=12, log=H.LOG):
    deadline = time.time() + timeout
    last_len = len(H.server_log(log))
    while time.time() < deadline:
        txt = H.server_log(log)
        i = txt.find(substr, max(0, last_len - 500000))
        if i >= 0:
            return True
        time.sleep(0.5)
    return H.server_log(log).find(substr) >= 0


def wait_log_absent(substr, timeout=6, log=H.LOG):
    time.sleep(timeout)
    return substr not in H.server_log(log)


# ---------------- X11-observable input checks ----------------

def x11_keymap_pressed(keysym_char="x"):
    from selkies.Xlib import XK
    code = None
    d = H.x_display()
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


def x11_mouse_pos():
    """Pointer position straight from the X server, so no xdotool is needed."""
    d = H.x_display()
    try:
        p = d.screen().root.query_pointer()
        return p.root_x, p.root_y
    finally:
        d.close()
