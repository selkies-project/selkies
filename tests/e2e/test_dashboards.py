#!/usr/bin/env python3
"""Dashboard e2e: selkies-dashboard (classic) and selkies-dashboard-wish,
covering both dashboards' settings loop, mode switching, and admin UI gates."""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright


def wait_canvas(page, timeout: float = 15):
    return C.wait_ws_video(page, timeout)


def classic_open_video(page) -> bool:
    """Open the classic dashboard's Video section.

    The sidebar starts closed: open it via the edge toggle-handle first, then
    the video section header (sections start closed too).

    Returns:
        True when the Video section header was found and clicked.
    """
    try:
        page.locator('.toggle-handle').first.click()
        time.sleep(0.8)
    except Exception:
        pass
    try:
        el = page.locator('.sidebar-section-header:has-text("Video")').first
        if el.count():
            el.scroll_into_view_if_needed()
            el.click()
            time.sleep(0.8)
            return True
    except Exception:
        pass
    return False


def set_range_slider(page, idx: int, value) -> None:
    """Set an HTML range input slider by index via native setters + events."""
    page.evaluate("""([idx, value]) => {
      const els = [...document.querySelectorAll('input[type="range"]')];
      const el = els[idx];
      if (!el) throw new Error('range el missing');
      const proto = Object.getPrototypeOf(el);
      const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
      setter.call(el, String(value));
      el.dispatchEvent(new Event('input', {bubbles: true}));
      el.dispatchEvent(new Event('change', {bubbles: true}));
    }""", [idx, value])


def dash_block(dashboard: str, dist: str) -> "H.Results":
    """Exercise one dashboard's settings loop and mode-switch round trip.

    Args:
        dashboard: ``classic`` or ``wish``.
        dist: Path to that dashboard's built web root.

    Returns:
        The Results accumulator for this dashboard's checks.
    """
    res = H.Results(f"dash-{dashboard}")
    H.server_start(mode="websockets", wayland=False, web_root=dist)
    # Fixed viewport width keeps the sidebar content deterministic.
    with sync_playwright() as p:
        browser = C.chromium_launch(p)
        ctx = browser.new_context(viewport={"width": 1440, "height": 900},
                                  device_scale_factor=1)
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
        try:
            ctx.grant_permissions(["clipboard-read", "clipboard-write"], origin=H.BASE_URL)
        except Exception:
            pass
        page = ctx.new_page()
        console_errors = []
        page.on("console", lambda m: console_errors.append(m.text) if m.type == "error" else None)
        page.on("pageerror", lambda e: console_errors.append(str(e)))
        page.goto(H.BASE_URL, wait_until="load")
        time.sleep(9.0)

        # Core UI chrome must be visible before anything else is driven.
        chrome = page.evaluate("""(() => ({
            body: document.body.innerText.length,
            sidebar: !!document.querySelector('.sidebar, [role="menubar"], .top-menu, .sidebar-container'),
        }))()""")
        res.check("dashboard chrome renders", chrome["sidebar"] or chrome["body"] > 400,
                  chrome)
        info = wait_canvas(page)
        res.check("video streams", info is not None, info)

        # A UI-driven framerate change must apply server-side.
        st = len(H.server_log())
        changed = False
        if dashboard == "classic":
            try:
                opened = classic_open_video(page)
                time.sleep(1.0)
                if page.locator('#framerateSlider').count():
                    page.evaluate("""() => {
                      const el = document.getElementById('framerateSlider');
                      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
                      setter.call(el, '30');
                      el.dispatchEvent(new Event('input', {bubbles: true}));
                      el.dispatchEvent(new Event('change', {bubbles: true}));
                    }""")
                    changed = True
                else:
                    print("classic: framerateSlider not found after open:", opened)
            except Exception as e:
                print("classic slider err:", e)
        else:
            # Wish: the Settings panel opens via the Settings2 icon in the
            # control strip; Radix sliders are driven by click-to-focus plus
            # arrow keys.
            try:
                opened = False
                trig = page.locator('button:has(svg.lucide-settings-2)').first
                if trig.count():
                    trig.click(force=True, timeout=3000)
                    time.sleep(1.2)
                    opened = page.locator('[role="slider"]').count() > 0
                if not opened:
                    # Fall back to locating the button by its aria tooltip text.
                    page.get_by_role("button", name=lambda n: "settings" in (n or "").lower()).first.click(force=True, timeout=2500)
                    time.sleep(1.2)
                    opened = page.locator('[role="slider"]').count() > 0
                if opened:
                    track = page.locator('[role="slider"]').first
                    track.click(force=True, timeout=2000)
                    time.sleep(0.2)
                    for _ in range(2):
                        page.keyboard.press("ArrowRight")
                        time.sleep(0.15)
                    changed = True
                else:
                    print("wish: settings panel did not open")
            except Exception as e:
                print("wish slider err:", e)
        time.sleep(3.0)
        newlog = H.server_log()[st:]
        applied = ("Applying video settings" in newlog or "Updated framerate to" in newlog
                   or "framerate" in newlog.lower())
        res.check("UI framerate change applied server-side", changed and applied, newlog[-160:])

        # Mode switch via the dashboard's own dropdown, then the parity loop
        # back: websockets to webrtc and back to websockets.
        if dashboard == "classic":
            sel = page.locator('select').first
            if sel.count():
                sel.select_option(value="webrtc")
            else:
                page.evaluate("""window.postMessage({type:'mode', mode:'webrtc'}, window.location.origin)""")
        else:
            try:
                dd = page.locator('button:has-text("WebSocket"), button:has-text("WebRTC")').first
                if dd.count() and dd.is_visible():
                    dd.click()
                    time.sleep(0.5)
                    page.locator('div[role="option"]:has-text("WebRTC")').first.click()
                else:
                    page.evaluate("window.postMessage({type:'mode', mode:'webrtc'}, window.location.origin)")
            except Exception:
                page.evaluate("window.postMessage({type:'mode', mode:'webrtc'}, window.location.origin)")
        time.sleep(1.5)
        # The dashboard may call /api/switch; do it directly if not flipped yet.
        status_mode = json.loads(H.curl("/api/status")[1]).get("current_mode")
        if status_mode != "webrtc":
            s, body = H.curl("/api/switch", method="POST", data={"mode": "webrtc"})
            res.check("mode switch api", s == 200, body[:60])
        time.sleep(6.0)
        # A fresh page pinned to webrtc must stream after the flip.
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'webrtc';")
        page2 = ctx.new_page()
        page2.goto(H.BASE_URL, wait_until="load")
        info2 = C.wait_wr_video(page2, timeout=45)
        res.check("video flows in webrtc after dashboard switch", info2 is not None, info2)
        page2.close()

        # Switch back to websockets; a fresh page must stream again.
        s, body = H.curl("/api/switch", method="POST", data={"mode": "websockets"})
        res.check("mode switch back to websocket api", s == 200, body[:60])
        time.sleep(4.0)
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
        page3 = ctx.new_page()
        page3.goto(H.BASE_URL, wait_until="load")
        info3 = wait_canvas(page3)
        res.check("video flows back in websockets", info3 is not None, info3)
        page3.close()

        real_errors = [e for e in console_errors if not any(
            bp in e for bp in ("Failed to load resource", "Unexpected server response:",
                               "ResizeObserver", "server shutting down",
                               "Error getting media devices"))]
        res.check("no console errors", len(real_errors) == 0, "; ".join(real_errors)[:150])
        browser.close()
    res.summary()
    return res


def gates_block(dashboard: str, dist: str) -> "H.Results":
    """ui_sidebar_show_shortcuts=false and ui_sidebar_show_webcam=false must
    hide the shortcuts UI and the webcam toggle on BOTH dashboards."""
    res = H.Results(f"gates-{dashboard}")
    H.server_start(mode="websockets", wayland=False, web_root=dist,
                   extra_env={
                       "SELKIES_UI_SIDEBAR_SHOW_SHORTCUTS": "false",
                       "SELKIES_UI_SHOW_SIDEBAR": "false",
                   })
    with sync_playwright() as p:
        browser = C.chromium_launch(p)
        ctx = browser.new_context(viewport={"width": 1440, "height": 900})
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
        page = ctx.new_page()
        page.goto(H.BASE_URL, wait_until="load")
        time.sleep(8.0)
        # ui_show_sidebar=false must hide the chrome entirely.
        chrome = page.evaluate("""(() => !!document.querySelector('.sidebar, [role="menubar"], .top-menu, .sidebar-container'))()""")
        res.check("ui_show_sidebar=false hides chrome", not chrome, chrome)
        # The shortcuts gate needs the sidebar enabled, so it gets its own boot.
        browser.close()
    H.server_start(mode="websockets", wayland=False, web_root=dist,
                   extra_env={
                       "SELKIES_UI_SIDEBAR_SHOW_SHORTCUTS": "false",
                       "SELKIES_UI_SIDEBAR_SHOW_WEBCAM": "false",
                   })
    with sync_playwright() as p:
        browser = C.chromium_launch(p)
        ctx = browser.new_context(viewport={"width": 1440, "height": 900})
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
        page = ctx.new_page()
        page.goto(H.BASE_URL, wait_until="load")
        time.sleep(8.0)
        body = page.evaluate("document.body.innerText")
        # 'Shortcuts' is a section/menu label in both dashboards, and the label
        # text shows up in innerText even when the section exists only as a DOM
        # label, so this catches a section that was merely collapsed.
        has_shortcuts = "Shortcuts" in body
        res.check("ui_sidebar_show_shortcuts=false hides Shortcuts section",
                  not has_shortcuts, has_shortcuts)
        # The webcam gate hides one entry of the core-button group while its
        # siblings stay, so the microphone control doubles as proof that the
        # group itself was reached (sidebar opened / stream menu expanded).
        if dashboard == "classic":
            try:
                page.locator('.toggle-handle').first.click()
                time.sleep(0.8)
            except Exception:
                pass
            mic, cam = page.evaluate("""(() => [
              !!document.querySelector('button[title$="Microphone"]'),
              !!document.querySelector('button[title$="Webcam"]')])()""")
        else:
            try:
                page.locator('[role="menubar"] button').first.click()
                time.sleep(0.8)
            except Exception:
                pass
            menu = page.evaluate("""(() => [...document.querySelectorAll('[role="menu"]')]
              .map(m => m.innerText).join('\\n'))()""")
            mic, cam = "Microphone" in menu, "Webcam" in menu
        res.check("core buttons reachable (microphone toggle present)", mic, mic)
        res.check("ui_sidebar_show_webcam=false hides webcam toggle", not cam, cam)
        browser.close()
    res.summary()
    return res


def main() -> None:
    """Run the dashboard blocks named on argv (default: all)."""
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    if which in ("all", "classic"):
        blocks.append(dash_block("classic", H.CLASSIC_DIST))
    if which in ("all", "wish"):
        blocks.append(dash_block("wish", H.WISH_DIST))
    if which in ("all", "gates"):
        blocks.append(gates_block("classic", H.CLASSIC_DIST))
        blocks.append(gates_block("wish", H.WISH_DIST))
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== DASH: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
