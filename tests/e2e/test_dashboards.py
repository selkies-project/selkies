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


def wish_open_menu_item(page, label: str) -> bool:
    """Open the Wish menubar menu that carries `label` and click that item.

    The top menu is several Radix menubars; each is opened in turn until one
    renders a menu item with the label (a submenu trigger counts).

    Returns:
        True when the item was found and clicked.
    """
    triggers = page.locator('[role="menubar"] button')
    for i in range(triggers.count()):
        try:
            triggers.nth(i).click()
            time.sleep(0.5)
            item = page.locator(f'[role="menu"] [role="menuitem"]:has-text("{label}")').first
            if item.count():
                item.click()
                time.sleep(0.8)
                return True
            page.keyboard.press("Escape")
            time.sleep(0.2)
        except Exception:
            pass
    return False


def wish_clipboard_seed_check(page, res: "H.Results") -> None:
    """A server clipboard change that arrives while the Wish Clipboard panel
    is closed must show in the textarea when the panel is opened: the panel
    mounts lazily, so it seeds from the last clipboardContentUpdate."""
    push = f"e2e-wish-clip-{int(time.time())}"
    _, stop = H.x_own_clipboard(push.encode())
    got = []
    deadline = time.time() + 8
    while time.time() < deadline:
        got = page.evaluate("window.__clipMsgs.map(m => m.text)")
        if push in got:
            break
        page.wait_for_timeout(500)
    stop["flag"] = True
    res.check("clipboard event reached the page while the panel was closed",
              push in got, repr(got)[-120:])
    opened = wish_open_menu_item(page, "Clipboard")
    shown = ""
    if opened:
        try:
            area = page.locator('#dashboardClipboardTextarea')
            area.wait_for(state="visible", timeout=4000)
            shown = area.input_value()
        except Exception as e:
            shown = f"<no textarea: {e}>"
    res.check("clipboard panel opens with the text that arrived while closed",
              opened and shown == push, shown[:80])
    page.keyboard.press("Escape")
    time.sleep(0.3)


def classic_viewer_check(page, res: "H.Results") -> None:
    """A client demoted to viewer by the server gets no classic sidebar: an
    open sidebar folds, the handle goes, and Ctrl+Shift+M (the core's own
    chord, and the toggleDashboard message it posts) opens nothing."""
    is_open = "!!document.querySelector('.sidebar.is-open')"
    # Positive control: the chord opens and closes the sidebar for a controller.
    page.mouse.click(700, 450)
    time.sleep(0.3)
    page.keyboard.press("Control+Shift+M")
    time.sleep(0.8)
    chord_opens = page.evaluate(is_open)
    res.check("Ctrl+Shift+M opens the sidebar for a controller", chord_opens, chord_opens)
    if not chord_opens:
        try:
            page.locator('.toggle-handle').first.click()
            time.sleep(0.6)
        except Exception:
            pass
    page.evaluate("window.postMessage({type: 'clientRoleUpdate', role: 'viewer'}, window.location.origin)")
    time.sleep(0.6)
    folded = not page.evaluate(is_open)
    handle = page.locator('.toggle-handle').count()
    res.check("viewer demotion folds the sidebar and removes the handle",
              folded and handle == 0, f"open={not folded} handle={handle}")
    page.keyboard.press("Control+Shift+M")
    time.sleep(0.6)
    page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
    time.sleep(0.6)
    reopened = page.evaluate(is_open)
    res.check("Ctrl+Shift+M does not open the sidebar for a viewer", not reopened, reopened)


def gaming_mode_check(page, res: "H.Results", dashboard: str) -> None:
    """Gaming mode stays reachable on a touch client, and by its chord.

    The header carries the fullscreen and gaming-mode pair on every client;
    the trackpad toggle joins the keyboard tile in the action-button row, and
    appears only once touch is seen. The Ctrl+Shift+X chord is the core's own,
    so it works whatever the dashboard shows.
    """
    if dashboard == "classic":
        present = lambda: page.evaluate("""() => ({
            gaming: !!document.querySelector('.header-controls .gaming-mode-button'),
            trackpad: !!document.querySelector('.sidebar-action-buttons .trackpad-mode-button'),
            headerTrackpad: !!document.querySelector('.header-controls .trackpad-mode-button'),
        })""")
    else:
        present = lambda: page.evaluate("""() => ({
            gaming: !!document.querySelector('button:has(svg.lucide-crosshair)'),
            trackpad: !!document.querySelector('.fixed.bottom-4 button:has(svg.lucide-touchpad)'),
            headerTrackpad: !!document.querySelector('[data-slot="tooltip-trigger"] button:has(svg.lucide-touchpad)'),
        })""")
    before = present()
    page.evaluate("window.dispatchEvent(new TouchEvent('touchstart', {bubbles: true}))")
    time.sleep(1.0)
    after = present()
    res.check("gaming mode button shown without touch", before["gaming"], before)
    res.check("gaming mode button survives touch detection", after["gaming"], after)
    res.check("trackpad button appears with touch, in the action row", after["trackpad"], after)
    res.check("the header carries no trackpad button", not after["headerTrackpad"], after)

    # requestFullscreen needs a real display; the mode the input handler
    # publishes is what the chord has to move.
    page.evaluate("""() => {
      Element.prototype.requestFullscreen = function () { return Promise.resolve(); };
      Document.prototype.requestFullscreen = function () { return Promise.resolve(); };
      window.__gaming = [];
      window.addEventListener('message', (e) => {
        if (e.data && e.data.type === 'gamingModeUpdate') window.__gaming.push(e.data.active);
      });
    }""")
    page.keyboard.press("Control+Shift+X")
    time.sleep(1.0)
    entered = page.evaluate("!!(window.webrtcInput && window.webrtcInput.gamingMode)")
    page.keyboard.press("Control+Shift+X")
    time.sleep(1.0)
    left = page.evaluate("!!(window.webrtcInput && window.webrtcInput.gamingMode)")
    posted = page.evaluate("window.__gaming")
    res.check("Ctrl+Shift+X enters gaming mode", entered, posted)
    res.check("Ctrl+Shift+X leaves gaming mode", entered and not left, posted)


def classic_layout_check(page, res: "H.Results") -> None:
    """Pin the classic sidebar layout: header icon parity, uniform tiles,
    and a files modal whose close control sits inside the panel.

    Runs after gaming_mode_check: the touchstart it dispatches is what makes
    the keyboard and trackpad tiles render, so the action row is at its
    fullest seven here and has to wrap without resizing any tile.
    """
    if not page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.8)

    icons = page.evaluate("""() => {
      const ink = (svg, strokeUnits) => {
        const b = svg.getBBox();
        const scale = svg.clientWidth / svg.viewBox.baseVal.width;
        return {
          box: svg.clientWidth,
          w: (b.width + strokeUnits) * scale,
          h: (b.height + strokeUnits) * scale,
        };
      };
      const fs = document.querySelector('.fullscreen-button svg');
      const gm = document.querySelector('.gaming-mode-button svg');
      if (!fs || !gm) return null;
      return { fs: ink(fs, 0), gm: ink(gm, 2) };
    }""")
    same = (icons and icons["fs"]["box"] == icons["gm"]["box"]
            and abs(icons["fs"]["w"] - icons["gm"]["w"]) <= 1.0
            and abs(icons["fs"]["h"] - icons["gm"]["h"]) <= 1.0)
    res.check("fullscreen and gaming icons draw at one size", same, icons)

    tiles = page.evaluate("""() => {
      const els = [...document.querySelectorAll('.sidebar-action-buttons .action-button')];
      const r = els.map(e => e.getBoundingClientRect());
      return {
        n: els.length,
        widths: r.map(x => Math.round(x.width * 10) / 10),
        rows: [...new Set(r.map(x => Math.round(x.top)))].length,
        keyRow: [...document.querySelectorAll('.sidebar-mobile-key-actions .mobile-key-button')].length,
        keyRowIcons: document.querySelectorAll('.sidebar-mobile-key-actions svg').length,
      };
    }""")
    uniform = (tiles["n"] == 7 and tiles["rows"] == 2
               and max(tiles["widths"]) - min(tiles["widths"]) <= 1.0)
    res.check("seven action tiles wrap onto two rows at one width", uniform, tiles)
    res.check("the key row holds the five soft keys and no icon buttons",
              tiles["keyRow"] == 5 and tiles["keyRowIcons"] == 0, tiles)

    opened = False
    try:
        page.locator('.sidebar-section-header:has-text("Files")').first.click()
        time.sleep(0.6)
        page.locator('button[title="Download Files"]').first.click()
        time.sleep(2.5)
        opened = page.locator('.files-modal').count() > 0
    except Exception:
        pass
    if not opened:
        res.check("files modal opens", False, "no .files-modal")
        return
    modal = page.evaluate("""() => {
      const m = document.querySelector('.files-modal').getBoundingClientRect();
      const c = document.querySelector('.files-modal-close').getBoundingClientRect();
      const f = document.querySelector('.files-modal iframe').getBoundingClientRect();
      let iframeBg = null, iframeTheme = null;
      try {
        const doc = document.querySelector('.files-modal iframe').contentDocument;
        iframeBg = doc && doc.body ? getComputedStyle(doc.body).backgroundColor : null;
        iframeTheme = doc ? doc.documentElement.dataset.theme || null : null;
      } catch (e) { iframeBg = 'err:' + e; }
      return {
        clear: c.right <= m.right - 4 && c.top >= m.top + 2 && c.bottom <= f.top,
        iframeBg, iframeTheme,
        dashTheme: document.querySelector('.sidebar').className.includes('theme-dark') ? 'dark' : 'light',
      };
    }""")
    res.check("files close button sits inside the panel above the frame",
              modal["clear"], modal)
    palettes = {"dark": "rgb(18, 22, 29)", "light": "rgb(244, 245, 248)"}
    res.check("file index follows the dashboard theme",
              modal["iframeTheme"] == modal["dashTheme"]
              and modal["iframeBg"] == palettes.get(modal["dashTheme"]), modal)

    # The mirror is live: the dashboard's own toggle re-renders the modal
    # frame while its storage write restyles the page inside the frame.
    page.locator('.theme-toggle').click()
    time.sleep(0.6)
    flipped = page.evaluate("""() => {
      const out = {modalBg: getComputedStyle(document.querySelector('.files-modal')).backgroundColor};
      try {
        const doc = document.querySelector('.files-modal iframe').contentDocument;
        out.theme = doc.documentElement.dataset.theme;
        out.bg = getComputedStyle(doc.body).backgroundColor;
      } catch (e) { out.theme = 'err'; out.bg = '' + e; }
      return out;
    }""")
    res.check("frame and file index follow a live theme flip",
              flipped["theme"] == "light" and flipped["bg"] == palettes["light"]
              and flipped["modalBg"] == "rgb(255, 255, 255)", flipped)
    page.locator('.theme-toggle').click()
    time.sleep(0.4)
    page.locator('.files-modal-close').click()
    time.sleep(0.4)
    # Leave the sidebar as found: the blocks after this one open it themselves.
    if page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.6)


# A 1x1 PNG, the smallest thing the image path accepts.
CLIPBOARD_PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d494844520000000100000001080600000"
    "01f15c4890000000a49444154789c6360000002000100ffff03000006000557bfabd40000000049454e44ae426082")


def clipboard_image_check(page, res: "H.Results", dashboard: str) -> None:
    """A picked image reaches the core as a blob, and anything else is refused.

    The upload button is the only way binary clipboard content leaves the
    client, and Wish previews what was picked: a preview showing anything but
    the `blob:` URL the dashboard minted would be rendering a URL from
    somewhere else entirely.
    """
    page.evaluate("""() => {
      window.__clipImages = [];
      window.__clipRefusals = [];
      window.addEventListener('message', (e) => {
        const d = e.data;
        if (!d || typeof d !== 'object') return;
        if (d.type === 'clipboardImageUpdate') {
          window.__clipImages.push(d.imageBlob ? d.imageBlob.size : 0);
        }
        if (d.type === 'fileUpload' && d.payload && d.payload.status === 'warning') {
          window.__clipRefusals.push(d.payload.fileName || '');
        }
      });
    }""")
    was_open = page.evaluate("!!document.querySelector('.sidebar.is-open')")
    if dashboard == "classic":
        if not was_open:
            page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
            time.sleep(0.8)
        page.locator('.sidebar-section-header:has-text("Clipboard")').first.click()
        time.sleep(0.6)
    elif not wish_open_menu_item(page, "Clipboard"):
        res.skip(f"{dashboard}: the clipboard image path", "no clipboard panel opened")
        return

    picker = page.locator('input[type="file"][accept="image/*"]').first
    if picker.count() == 0:
        res.skip(f"{dashboard}: the clipboard image path", "no image picker in the panel")
        return
    picker.set_input_files({"name": "clip.png", "mimeType": "image/png", "buffer": CLIPBOARD_PNG})
    time.sleep(0.8)
    res.check(f"{dashboard}: a picked image reaches the core whole",
              page.evaluate("window.__clipImages") == [len(CLIPBOARD_PNG)],
              page.evaluate("window.__clipImages"))
    previews = page.locator('img[src^="blob:"]').count()
    if dashboard == "wish":
        res.check("wish: the preview shows the blob it minted", previews == 1, previews)
    else:
        res.check("classic: the panel previews nothing to mint a URL for", previews == 0, previews)

    picker.set_input_files({"name": "clip.txt", "mimeType": "text/plain", "buffer": b"not an image"})
    time.sleep(0.8)
    res.check(f"{dashboard}: anything but an image is refused, not sent",
              page.evaluate("window.__clipImages") == [len(CLIPBOARD_PNG)]
              and page.evaluate("window.__clipRefusals.length") == 1,
              page.evaluate("[window.__clipImages, window.__clipRefusals]"))
    if dashboard == "classic":
        # Leave the sidebar as found: the checks after this one open it themselves.
        page.locator('.sidebar-section-header:has-text("Clipboard")').first.click()
        time.sleep(0.4)
        if not was_open and page.evaluate("!!document.querySelector('.sidebar.is-open')"):
            page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
            time.sleep(0.6)
    else:
        page.keyboard.press("Escape")
        time.sleep(0.3)


def touch_gamepad_layer_check(page, res: "H.Results", dashboard: str) -> None:
    """The touch gamepad is drawn over the whole viewport, so the dashboard's own
    panels have to sit above it: two controls stacked on the same pixels compete
    for the same tap, and the one that wins is the one nobody can see."""
    page.evaluate("""() => window.postMessage({
      type: 'TOUCH_GAMEPAD_SETUP',
      payload: { targetDivId: 'touch-gamepad-host', visible: true },
    }, window.location.origin)""")
    time.sleep(1.2)
    shown = page.evaluate(
        "() => document.querySelectorAll('#universal-touch-gamepad-controls-overlay *').length")
    res.check(f"{dashboard}: the touch gamepad draws its controls", shown > 0, shown)

    panel = ".sidebar"
    if dashboard == "classic":
        if not page.evaluate("!!document.querySelector('.sidebar.is-open')"):
            page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
            time.sleep(0.8)
    else:
        # Wish portals its panels to the body rather than into the dashboard
        # root, so they are the half of its chrome that has to clear the pad on
        # its own footing. Its menus sit at the top out of the pad's way; the
        # download modal is the one drawn over it.
        panel = '[data-slot="dialog-overlay"]'
        opened = wish_open_menu_item(page, "Files")
        if opened:
            try:
                page.locator('button:has-text("Download Files")').first.click()
                time.sleep(1.5)
            except Exception as e:
                print(f"      (wish download modal: {e!r})")
                opened = False
        if not opened or page.locator(panel).count() == 0:
            res.skip(f"{dashboard}: a panel over the touch gamepad", "no panel opened")
            page.evaluate("""() => window.postMessage({
              type: 'TOUCH_GAMEPAD_VISIBILITY',
              payload: { visible: false, targetDivId: 'touch-gamepad-host' },
            }, window.location.origin)""")
            return

    # elementsFromPoint reports the whole hit-test stack, so one sample says
    # both that the two overlap there and which of them takes the press.
    hits = page.evaluate("""(selector) => {
      const panel = document.querySelector(selector);
      const pad = document.getElementById('universal-touch-gamepad-controls-overlay');
      const pr = panel.getBoundingClientRect();
      const stacks = { contested: [], padOnly: 0,
                       covers: pr.left <= 0 && pr.top <= 0
                               && pr.right >= innerWidth && pr.bottom >= innerHeight };
      for (const el of pad.querySelectorAll('*')) {
        const r = el.getBoundingClientRect();
        if (!r.width || !r.height) continue;
        const stack = document.elementsFromPoint(
          Math.round(r.left + r.width / 2), Math.round(r.top + r.height / 2));
        const overPanel = stack.findIndex((e) => panel.contains(e) || e === panel);
        const overPad = stack.findIndex((e) => pad.contains(e));
        if (overPanel >= 0 && overPad >= 0) stacks.contested.push(overPanel < overPad ? 'panel' : 'pad');
        else if (overPad >= 0) stacks.padOnly++;
      }
      return stacks;
    }""", panel)
    res.check(f"{dashboard}: an open panel takes the taps of the controls under it",
              hits["contested"] and all(w == "panel" for w in hits["contested"]),
              f"{hits['contested'][:8]} contested, {hits['padOnly']} clear")
    if hits["covers"]:
        res.skip(f"{dashboard}: controls clear of it still take their own",
                 "the panel covers the viewport")
    else:
        res.check(f"{dashboard}: controls clear of it still take their own",
                  hits["padOnly"] > 0, hits["padOnly"])

    page.evaluate("""() => window.postMessage({
      type: 'TOUCH_GAMEPAD_VISIBILITY',
      payload: { visible: false, targetDivId: 'touch-gamepad-host' },
    }, window.location.origin)""")
    time.sleep(0.5)
    if dashboard == "classic":
        # Leave the sidebar as found, like the layout check before it.
        if page.evaluate("!!document.querySelector('.sidebar.is-open')"):
            page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
            time.sleep(0.6)
    else:
        page.keyboard.press("Escape")
        time.sleep(0.3)


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
        # has_touch stands in for a 2-in-1, whose touchscreen sits alongside a
        # keyboard and mouse: the header still has to offer gaming mode there.
        ctx = browser.new_context(viewport={"width": 1440, "height": 900},
                                  device_scale_factor=1, has_touch=True)
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
        # Records the core's clipboard postMessages so a check can tell that a
        # server push arrived before a panel was opened.
        ctx.add_init_script("""
          window.__clipMsgs = [];
          window.addEventListener('message', (e) => {
            if (e.data && e.data.type === 'clipboardContentUpdate') window.__clipMsgs.push(e.data);
          });
        """)
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

        chrome = page.evaluate("""(() => ({
            body: document.body.innerText.length,
            sidebar: !!document.querySelector('.sidebar, [role="menubar"], .top-menu, .sidebar-container'),
        }))()""")
        res.check("dashboard chrome renders", chrome["sidebar"] or chrome["body"] > 400,
                  chrome)
        info = wait_canvas(page)
        res.check("video streams", info is not None, info)

        # Positive control for the gates block below.
        if dashboard == "classic":
            section = page.locator('.sidebar-section-header:has-text("Gamepads")').count() > 0
        else:
            section = page.locator('text=/^Gamepad \\d+$/').count() > 0
        res.check("gamepads section shown by default", section, section)

        if dashboard == "wish":
            wish_clipboard_seed_check(page, res)
        gaming_mode_check(page, res, dashboard)
        if dashboard == "classic":
            classic_layout_check(page, res)
        touch_gamepad_layer_check(page, res, dashboard)
        clipboard_image_check(page, res, dashboard)

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
            # Radix sliders are driven by click-to-focus plus arrow keys.
            try:
                opened = False
                trig = page.locator('button:has(svg.lucide-settings-2)').first
                if trig.count():
                    trig.click(force=True, timeout=3000)
                    time.sleep(1.2)
                    opened = page.locator('[role="slider"]').count() > 0
                if not opened:
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
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'webrtc';")
        page2 = ctx.new_page()
        page2.goto(H.BASE_URL, wait_until="load")
        info2 = C.wait_wr_video(page2, timeout=45)
        res.check("video flows in webrtc after dashboard switch", info2 is not None, info2)
        page2.close()

        s, body = H.curl("/api/switch", method="POST", data={"mode": "websockets"})
        res.check("mode switch back to websocket api", s == 200, body[:60])
        time.sleep(4.0)
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
        page3 = ctx.new_page()
        page3.goto(H.BASE_URL, wait_until="load")
        info3 = wait_canvas(page3)
        res.check("video flows back in websockets", info3 is not None, info3)
        # On the fresh page (its input context is attached, so the chord is
        # live), and last: the demotion leaves the page without a sidebar.
        if dashboard == "classic":
            classic_viewer_check(page3, res)
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
    hide the shortcuts UI and the webcam toggle on BOTH dashboards;
    ui_sidebar_show_gamepads=false hides the gamepads section (the visualizer
    card in Wish) and nothing else, so the gamepad input toggle stays."""
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
        chrome = page.evaluate("""(() => !!document.querySelector('.sidebar, [role="menubar"], .top-menu, .sidebar-container'))()""")
        res.check("ui_show_sidebar=false hides chrome", not chrome, chrome)
        # The shortcuts gate needs the sidebar enabled, so it gets its own boot.
        browser.close()
    H.server_start(mode="websockets", wayland=False, web_root=dist,
                   extra_env={
                       "SELKIES_UI_SIDEBAR_SHOW_SHORTCUTS": "false",
                       "SELKIES_UI_SIDEBAR_SHOW_WEBCAM": "false",
                       "SELKIES_UI_SIDEBAR_SHOW_GAMEPADS": "false",
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
            mic, cam, pad = page.evaluate("""(() => [
              !!document.querySelector('button[title$="Microphone"]'),
              !!document.querySelector('button[title$="Webcam"]'),
              !!document.querySelector('button[title$="Gamepad Input"]')])()""")
            section = page.locator('.sidebar-section-header:has-text("Gamepads")').count() > 0
        else:
            menu = ""
            triggers = page.locator('[role="menubar"] button')
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
            mic, cam, pad = "Microphone" in menu, "Webcam" in menu, "Gamepad Input" in menu
            # The card renders a titled visualizer ("Gamepad 0") when shown.
            section = page.locator('text=/^Gamepad \\d+$/').count() > 0
        res.check("core buttons reachable (microphone toggle present)", mic, mic)
        res.check("ui_sidebar_show_webcam=false hides webcam toggle", not cam, cam)
        res.check("ui_sidebar_show_gamepads=false keeps the gamepad input toggle", pad, pad)
        res.check("ui_sidebar_show_gamepads=false hides the gamepads section", not section, section)
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
