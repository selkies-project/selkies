#!/usr/bin/env python3
"""The dashboard on the transport/backend cells test_dashboards.py does not
reach: the toolbar layout on WebRTC and on Wayland, and both layouts' admin
UI gates on WebRTC and Wayland.

wish-webrtc-x11 / wish-ws-wl / wish-webrtc-wl:
    The toolbar renders its chrome and streams; the gamepads panel is shown
    by default; a server clipboard change that arrives while the Clipboard
    panel is closed seeds the panel, and text typed into the panel reaches the
    server's selection (X CLIPBOARD or the compositor's); the settings card's
    framerate slider and encoder menu each produce a change the server
    acknowledges in its log, with video still arriving after the encoder
    restart; the Apps panel opens and, with remote commands off, says so.
gates-webrtc-x11 / gates-ws-wl / gates-webrtc-wl:
    ui_show_sidebar=false hides the chrome of both layouts;
    ui_sidebar_show_shortcuts / _webcam / _gamepads=false hide the Shortcuts
    section, the webcam toggle, and the gamepads section (the pads part of
    the toolbar's panel) while the microphone and gamepad-input toggles stay.

Usage: python3 tests/e2e/test_dashboard_matrix.py [<cell>|all]
"""
import os
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_dashboards as TD
from playwright.sync_api import sync_playwright

WL_SOCKET = "wayland-1"
CELLS = ("wish-webrtc-x11", "wish-ws-wl", "wish-webrtc-wl",
         "gates-webrtc-x11", "gates-ws-wl", "gates-webrtc-wl")
# Encoders in the order a switch away from the default prefers them: the
# striped H.264 mode keeps the WebCodecs path, JPEG needs no decoder at all.
ENCODER_PREFERENCE = ["h264enc-striped", "jpeg", "h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc"]

# Records the core's clipboard messages (a push that arrives while the panel
# is closed) and the server settings payload the dashboard renders from.
PAGE_JS = """
  window.__clipMsgs = [];
  window.__serverSettings = null;
  window.addEventListener('message', (e) => {
    if (!e.data || !e.data.type) return;
    if (e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
    if (e.data.type === 'serverSettings') window.__serverSettings = e.data.payload;
  });
"""


def cell_parts(cell: str) -> tuple:
    """`(mode, wayland)` for a cell name ending in `-{ws|webrtc}-{x11|wl}`."""
    _, transport, backend = cell.split("-")
    return ("webrtc" if transport == "webrtc" else "websockets"), backend == "wl"


def wait_video(page: Any, mode: str, timeout: float = 45) -> Optional[dict]:
    return C.wait_wr_video(page, timeout) if mode == "webrtc" else C.wait_ws_video(page, timeout)


def new_video_frames(page: Any, mode: str, timeout: float = 10) -> int:
    """Video units that arrive from now on: WS chunks or decoded WebRTC frames."""
    expr = ("window.videoChunksReceived || 0" if mode == "websockets"
            else "(() => { const v = document.querySelector('video');"
                 " return v && v.getVideoPlaybackQuality ? v.getVideoPlaybackQuality().totalVideoFrames : 0; })()")
    before = page.evaluate(expr)
    deadline = time.time() + timeout
    while time.time() < deadline:
        now = page.evaluate(expr)
        if now > before:
            return now - before
        time.sleep(0.5)
    return 0


def server_push_clipboard(text: str, wayland: bool) -> Any:
    """Change the server-side selection; returns what stops an X owner thread."""
    if wayland:
        H.wl_copy(WL_SOCKET, text)
        return None
    _, stop = H.x_own_clipboard(text.encode())
    return stop


def server_clipboard(wayland: bool) -> Optional[str]:
    return H.wl_paste(WL_SOCKET).strip() if wayland else H.x_read_clipboard(timeout=3)


def log_since(mark: int, substr: str, timeout: float = 12) -> bool:
    """Whether `substr` shows up in the server log written after `mark`."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if substr in H.server_log()[mark:]:
            return True
        time.sleep(0.4)
    return False


def open_settings(page: Any) -> bool:
    """Open the toolbar's settings card on its Video tab, whose frame rate
    slider is what the block drives."""
    if not TD.open_wish_settings_tab(page, "Video"):
        return False
    deadline = time.time() + 6
    while time.time() < deadline:
        if page.locator('#framerateSlider').count() > 0:
            return True
        time.sleep(0.3)
    return False


def app_runner_stub_path() -> str:
    """A PATH dir with a `selkies-proot` that answers `check`, so the apps
    panel publishes while remote commands stay disabled (the default here)."""
    stub_dir = os.path.join(H.WORKDIR, "dash-cmd-stub")
    os.makedirs(stub_dir, exist_ok=True)
    stub = os.path.join(stub_dir, "selkies-proot")
    with open(stub, "w") as fh:
        fh.write('#!/bin/sh\n[ "$1" = check ] && exit 0\nexit 1\n')
    os.chmod(stub, 0o755)
    return stub_dir


def wish_block(cell: str) -> "H.Results":
    """One toolbar cell: stream, clipboard panel both ways, settings applied
    server-side, apps panel, gamepads panel."""
    mode, wayland = cell_parts(cell)
    res = H.Results(cell)
    H.server_start(mode=mode, wayland=wayland, web_root=H.DASHBOARD_DIST, layout="wish",
                   extra_env={"PATH": app_runner_stub_path() + os.pathsep
                              + os.environ.get("PATH", "")})
    with sync_playwright() as pw:
        browser = C.chromium_launch(pw)
        ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=1)
        ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
        ctx.add_init_script(PAGE_JS)
        try:
            ctx.grant_permissions(["clipboard-read", "clipboard-write"], origin=H.BASE_URL)
        except Exception:
            pass
        page = ctx.new_page()
        console_errors = []
        page.on("console", lambda m: console_errors.append(m.text) if m.type == "error" else None)
        page.on("pageerror", lambda e: console_errors.append(str(e)))
        page.goto(H.BASE_URL, wait_until="load")
        try:
            info = wait_video(page, mode)
            res.check("video streams", info is not None, info)
            # The chrome arrives with the page; the Gamepads panel's pads part
            # follows the server settings payload.
            deadline = time.time() + 10
            card = False
            while time.time() < deadline and not card:
                card = TD.wish_gamepad_panel(page)["pads"]
                time.sleep(0.5)
            res.check("dashboard chrome renders", page.locator(TD.WISH_CHROME).count() > 0)
            res.check("gamepad card shown by default", card)

            push = f"e2e-{cell}-s2c-{int(time.time())}"
            stop = server_push_clipboard(push, wayland)
            got = []
            deadline = time.time() + 8
            while time.time() < deadline:
                got = page.evaluate("window.__clipMsgs.map(m => m.text)")
                if push in got:
                    break
                page.wait_for_timeout(500)
            if stop is not None:
                stop["flag"] = True
            res.check("clipboard: server change reached the page while the panel was closed",
                      push in got, repr(got)[-100:])
            opened = TD.wish_open_menu_item(page, "Clipboard")
            shown = ""
            area = page.locator('#dashboardClipboardTextarea')
            if opened:
                try:
                    area.wait_for(state="visible", timeout=4000)
                    shown = area.input_value()
                except Exception as e:
                    shown = f"<no textarea: {e}>"
            res.check("clipboard: panel opens seeded with that text", opened and shown == push, shown[:80])
            probe = f"e2e-{cell}-c2s-{int(time.time())}"
            sent = False
            if opened and area.count():
                # The panel sends on blur; blurring the textarea in place keeps
                # the menu open, whereas a click elsewhere would close it.
                area.fill(probe)
                area.evaluate("el => el.blur()")
                sent = True
            got_text = None
            deadline = time.time() + 10
            while time.time() < deadline:
                got_text = server_clipboard(wayland)
                if got_text == probe:
                    break
                time.sleep(0.5)
            res.check("clipboard: text typed into the panel reached the server", sent and got_text == probe,
                      repr(got_text)[:80])
            TD.wish_close_menus(page)

            res.check("settings panel opens", open_settings(page))
            mark = len(H.server_log())
            slider = page.locator('#framerateSlider').first
            slider.focus()
            time.sleep(0.2)
            for _ in range(2):
                page.keyboard.press("ArrowLeft")
                time.sleep(0.15)
            line = ("Updated framerate to" if mode == "webrtc"
                    else "Applying video settings for 'primary' live (no restart)")
            res.check("settings: framerate slider change acknowledged by the server", log_since(mark, line),
                      H.server_log()[mark:][-120:])

            # The encoder menu renders only when the server offers a choice
            # (WebRTC streams h264enc alone, so there it is hidden).
            allowed = page.evaluate(
                "(window.__serverSettings && window.__serverSettings.encoder || {}).allowed || []")
            TD.open_wish_settings_tab(page, "Video")
            menu = page.locator('#encoderSelect')
            picked = None
            if len(allowed) <= 1:
                res.check(f"settings: no encoder menu when the server offers one encoder ({allowed})",
                          menu.count() == 0)
            elif menu.count() == 0:
                res.check("settings: the encoder menu is rendered", False, f"allowed={allowed}")
            else:
                current = menu.input_value()
                offered = page.evaluate("() => [...document.querySelectorAll('#encoderSelect option')]"
                                        ".filter((o) => !o.disabled).map((o) => o.value)")
                for value in ENCODER_PREFERENCE:
                    if value != current and value in offered:
                        mark = len(H.server_log())
                        menu.select_option(value, timeout=3000)
                        picked = value
                        break
                res.check("settings: the encoder menu offers another encoder", picked is not None,
                          f"current={current} offered={offered}")
            if picked:
                line = (f"encoder -> {picked}; restarting screen capture" if mode == "webrtc"
                        else "Video parameters changed for 'primary'. Restarting its capture stream")
                res.check(f"settings: encoder change to {picked} acknowledged by the server",
                          log_since(mark, line, timeout=15), H.server_log()[mark:][-120:])
                res.check("video still arrives after the encoder restart", new_video_frames(page, mode, 15) > 0)
            page.keyboard.press("Escape")
            time.sleep(0.3)

            opened = False
            notice = False
            if TD.wish_open_menu_item(page, "Apps"):
                deadline = time.time() + 6
                while time.time() < deadline and not opened:
                    opened = page.locator('[role="dialog"]:has-text("Apps")').count() > 0
                    time.sleep(0.3)
                body = page.locator('[role="dialog"]').first.inner_text() if opened else ""
                notice = "remote commands are disabled" in body
            res.check("apps panel opens", opened)
            res.check("apps panel says remote commands are disabled (command_enabled off)", notice)

            real_errors, _ = C.benign_console(console_errors, [])
            real_errors = [e for e in real_errors if not any(
                s in e for s in ("ResizeObserver", "Error getting media devices", "Wake Lock"))]
            res.check("no console errors (filtered)", not real_errors, "; ".join(real_errors)[:150])
        finally:
            browser.close()
    res.summary()
    return res


def gates_block(cell: str) -> "H.Results":
    """The admin UI gates on one transport/backend for both layouts."""
    mode, wayland = cell_parts(cell)
    res = H.Results(cell)
    for dashboard in H.LAYOUTS:
        H.server_start(mode=mode, wayland=wayland, web_root=H.DASHBOARD_DIST, layout=dashboard,
                       extra_env={"SELKIES_UI_SHOW_SIDEBAR": "false"})
        with sync_playwright() as pw:
            browser = C.chromium_launch(pw)
            ctx = browser.new_context(viewport={"width": 1440, "height": 900})
            ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            res.check(f"{dashboard}: video streams with the sidebar hidden", wait_video(page, mode) is not None)
            time.sleep(2.0)
            chrome = page.evaluate("""(() => !!document.querySelector('CHROME'))()""".replace("CHROME", TD.CHROME))
            res.check(f"{dashboard}: ui_show_sidebar=false hides chrome", not chrome, chrome)
            browser.close()
        H.server_start(mode=mode, wayland=wayland, web_root=H.DASHBOARD_DIST, layout=dashboard, extra_env={
            "SELKIES_UI_SIDEBAR_SHOW_SHORTCUTS": "false",
            "SELKIES_UI_SIDEBAR_SHOW_WEBCAM": "false",
            "SELKIES_UI_SIDEBAR_SHOW_GAMEPADS": "false",
        })
        with sync_playwright() as pw:
            browser = C.chromium_launch(pw)
            ctx = browser.new_context(viewport={"width": 1440, "height": 900})
            ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            res.check(f"{dashboard}: video streams", wait_video(page, mode) is not None)
            # The gates ride the server settings payload, which follows the
            # stream connection; give it a moment to render.
            time.sleep(3.0)
            if dashboard == "default":
                try:
                    page.locator('.toggle-handle').first.click()
                    time.sleep(0.8)
                except Exception:
                    pass
                body = page.evaluate("document.body.innerText")
                mic, cam, pad = page.evaluate("""(() => [
                  !!document.querySelector('button[title$="Microphone"]'),
                  !!document.querySelector('button[title$="Webcam"]'),
                  !!document.querySelector('button[title$="Gamepad Input"]')])()""")
                section = page.locator('.sidebar-section-header:has-text("Gamepads")').count() > 0
            else:
                body = page.evaluate("document.body.innerText")
                menu = ""
                triggers = page.locator(TD.WISH_MENU_TRIGGERS)
                for i in range(triggers.count()):
                    try:
                        triggers.nth(i).click()
                        time.sleep(0.6)
                        menu += page.evaluate("""(() => [...document.querySelectorAll('[role="menu"]')]
                          .map(m => m.innerText).join('\\n'))()""") + "\n"
                        page.keyboard.press("Escape")
                        time.sleep(0.2)
                    except Exception:
                        pass
                body += menu
                gamepads = TD.wish_gamepad_panel(page)
                mic, cam, pad = "Microphone" in menu, "Webcam" in menu, gamepads["input"]
                section = gamepads["pads"]
            res.check(f"{dashboard}: ui_sidebar_show_shortcuts=false hides the Shortcuts section",
                      "Shortcuts" not in body)
            res.check(f"{dashboard}: core buttons reachable (microphone toggle present)", mic)
            res.check(f"{dashboard}: ui_sidebar_show_webcam=false hides the webcam toggle", not cam)
            res.check(f"{dashboard}: ui_sidebar_show_gamepads=false keeps the gamepad input toggle", pad)
            res.check(f"{dashboard}: ui_sidebar_show_gamepads=false hides the gamepads section", not section)
            browser.close()
    res.summary()
    return res


def main() -> None:
    """Run the cells named on argv (default: all)."""
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which != "all" and which not in CELLS:
        print(f"unknown cell {which!r}; one of {', '.join(CELLS)}", file=sys.stderr)
        sys.exit(2)
    blocks = []
    try:
        for cell in CELLS:
            if which in ("all", cell):
                blocks.append(wish_block(cell) if cell.startswith("wish-") else gates_block(cell))
    finally:
        H.server_stop()
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== DASH-MATRIX: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
