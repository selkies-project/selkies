#!/usr/bin/env python3
"""Dashboard e2e: selkies-dashboard (classic) and selkies-dashboard-wish,
covering both dashboards' settings loop, mode switching, and admin UI gates."""
import json
import os
import re
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_dpi_accuracy as DA
from playwright.sync_api import sync_playwright


def wait_canvas(page, timeout: float = 15):
    return C.wait_ws_video(page, timeout)


def wait_chunk(page, timeout: float = 15) -> bool:
    """Wait for the first video chunk, by which the dashboard has the server's settings. Unlike
    `wait_canvas` it takes a canvas of any size: a phone's is under 640 pixels."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if page.evaluate("window.videoChunksReceived > 0"):
            return True
        time.sleep(0.25)
    return False


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


def classic_stats_strip_check(page, res: "H.Results") -> None:
    """The stats section's switch lays the strip over the stream, which keeps the
    numbers coming with the sidebar shut, shows the server's figures, takes no
    pointer input, and goes with the switch."""
    strip = """() => {
      const el = document.querySelector('.stream-strip');
      const latest = window.stream_stats && window.stream_stats.latest;
      return el ? { text: el.innerText, pointer: getComputedStyle(el).pointerEvents,
                    figures: el.querySelectorAll('.stream-strip-figure').length,
                    t: latest && latest.t, server: !!(latest && latest.server),
                    sent: !!(latest && typeof latest.send_ms === 'number') } : null;
    }"""
    try:
        page.locator('.toggle-handle').first.click()
        time.sleep(0.8)
        page.locator('.sidebar-section-header:has-text("Stats")').first.click()
        time.sleep(0.8)
        page.locator('#statsStripToggle').click()
        time.sleep(0.5)
        page.locator('.toggle-handle').first.click()
        time.sleep(4.0)
        first = page.evaluate(strip)
        time.sleep(2.5)
        later = page.evaluate(strip)
        res.check("the stats section's switch lays a strip over the stream that takes no pointer input",
                  bool(first) and first["figures"] >= 3 and first["pointer"] == "none", first)
        res.check("which keeps the server's figures coming with the sidebar shut, the time to the wire among them",
                  bool(later) and later["server"] and later["sent"] and later["t"] != first["t"], later)
        page.locator('.toggle-handle').first.click()
        time.sleep(0.8)
        page.locator('#statsStripToggle').click()
        time.sleep(0.5)
        gone = page.evaluate(strip)
        page.locator('.sidebar-section-header:has-text("Stats")').first.click()
        page.locator('.toggle-handle').first.click()
        time.sleep(0.8)
        res.check("and goes with the switch", gone is None, gone)
    except Exception as e:
        res.check("the stats strip check ran", False, str(e)[:200])


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


def wish_gamepad_preview(page) -> bool:
    """Whether the Wish gamepad dropdown carries the gamepad preview, a visualizer
    titled "Gamepad 0": each menubar menu is opened, read, and closed again."""
    triggers = page.locator('[role="menubar"] button')
    for i in range(triggers.count()):
        try:
            triggers.nth(i).click()
            time.sleep(0.5)
            found = page.locator('[role="menu"]').locator('text=/^Gamepad \\d+$/').count() > 0
            page.keyboard.press("Escape")
            time.sleep(0.2)
            if found:
                return True
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


def wish_stats_fit_check(page, res: "H.Results") -> None:
    """The detailed stats overlay fits a small window and reaches its last row
    by scrolling within, and it still drags.

    At 1280x720 the detailed view is taller than the window below its top, so
    without a height of its own its last rows sit below the window, where the
    drag clamp, pinning its top at 0, cannot bring them.
    """
    measure = """() => {
      const h3 = [...document.querySelectorAll('h3')].find((h) => h.closest('div.w-80'));
      if (!h3) return null;
      const panel = h3.closest('div.w-80');
      const scroller = [panel, ...panel.querySelectorAll('div')].find(
        (el) => /auto|scroll/.test(getComputedStyle(el).overflowY)) || panel;
      scroller.scrollTop = scroller.scrollHeight;
      const box = panel.getBoundingClientRect(), last = scroller.lastElementChild.getBoundingClientRect();
      return { left: Math.round(box.left), top: Math.round(box.top), bottom: Math.round(box.bottom),
               last_bottom: Math.round(last.bottom), shown_bottom: Math.round(scroller.getBoundingClientRect().bottom),
               viewport: innerHeight };
    }"""
    page.set_viewport_size({"width": 1280, "height": 720})
    try:
        page.mouse.move(640, 5)
        time.sleep(0.6)
        page.evaluate("() => document.querySelector('svg.lucide-gauge').closest('button').click()")
        time.sleep(0.5)
        page.evaluate("() => document.querySelector('svg.lucide-chevron-down').closest('button').click()")
        time.sleep(1.0)
        got = page.evaluate(measure)
        res.check("wish detailed stats fit a 720-px window and scroll to their last row",
                  bool(got) and got["bottom"] <= got["viewport"] and got["last_bottom"] <= got["shown_bottom"],
                  got)
        if got:
            page.mouse.move(got["left"] + 120, got["top"] + 14)
            page.mouse.down()
            page.mouse.move(got["left"] + 220, got["top"] + 4, steps=5)
            page.mouse.up()
            time.sleep(0.3)
            moved = page.evaluate(measure)
            res.check("wish detailed stats still drag, and stay in the window",
                      bool(moved) and moved["left"] == got["left"] + 100
                      and moved["bottom"] <= moved["viewport"], (got, moved))
        page.evaluate("() => document.querySelector('svg.lucide-gauge').closest('button').click()")
        time.sleep(0.5)
    finally:
        page.set_viewport_size({"width": 1440, "height": 900})


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
    settled = page.evaluate("""() => {
      const f = document.querySelector('.files-modal iframe');
      return {spinner: document.querySelectorAll('.files-modal-loading').length,
              opacity: getComputedStyle(f).opacity};
    }""")
    res.check("a loaded file index shows the frame and no spinner",
              settled == {"spinner": 0, "opacity": "1"}, settled)
    page.locator('.files-modal-close').click()
    time.sleep(0.4)

    # Watched rather than sampled: the frame loads from this same process in a
    # few milliseconds, far inside any poll interval, so the spinner is caught
    # by recording that it was mounted at all.
    page.evaluate("""() => {
      window.__spinner = {seen: false, label: null};
      const note = () => {
        const el = document.querySelector('.files-modal-loading');
        if (el) {
          window.__spinner.seen = true;
          const p = el.querySelector('p');
          window.__spinner.label = p ? p.textContent : '';
        }
      };
      new MutationObserver(note).observe(document.body, {childList: true, subtree: true});
      note();
    }""")
    page.locator('button[title="Download Files"]').first.click()
    time.sleep(2.5)
    spun = page.evaluate("""() => {
      const f = document.querySelector('.files-modal iframe');
      return {seen: window.__spinner.seen, label: window.__spinner.label,
              spinner: document.querySelectorAll('.files-modal-loading').length,
              opacity: f ? getComputedStyle(f).opacity : null};
    }""")
    res.check("opening the files modal shows a labeled spinner that the loaded frame clears",
              spun["seen"] and bool(spun["label"]) and spun["spinner"] == 0
              and spun["opacity"] == "1", spun)
    page.locator('.files-modal-close').click()
    time.sleep(0.4)
    # Leave the sidebar as found: the blocks after this one open it themselves.
    if page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.6)


# A 2x2 opaque red PNG: small enough to inline, and the color is what proves
# the preview decoded and drew the picked image rather than merely sizing a box.
CLIPBOARD_PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d4948445200000002000000020802000000fdd49a73"
    "0000001049444154789c63f8cfc000440c100a001fee03fd8b5f14d40000000049454e44ae426082")


def open_clipboard_panel(page, dashboard: str) -> bool:
    """Open the dashboard's clipboard panel; False when it did not open."""
    if dashboard == "classic":
        if not page.evaluate("!!document.querySelector('.sidebar.is-open')"):
            page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
            time.sleep(0.8)
        if page.locator('#dashboardClipboardTextarea').count() == 0:
            page.locator('.sidebar-section-header:has-text("Clipboard")').first.click()
            time.sleep(0.6)
        return page.locator('#dashboardClipboardTextarea').count() > 0
    return wish_open_menu_item(page, "Clipboard")


def pick_clipboard_image(page, dashboard: str, file: dict) -> bool:
    """Choose `file` through the panel's Upload Image button, as a user does.

    The browser's file dialog takes the window's focus while it is open, which
    closes any menu around the button (the Wish panel is a submenu). Playwright
    answers the dialog without showing it, so the blur the dialog would cause
    is dispatched between the click and the answer.

    Returns:
        False when the panel or its button is not there.
    """
    if not open_clipboard_panel(page, dashboard):
        return False
    button = page.locator('button:has-text("Upload Image")').first
    if button.count() == 0:
        return False
    with page.expect_file_chooser(timeout=5000) as chooser:
        button.click()
    page.evaluate("window.dispatchEvent(new Event('blur'))")
    time.sleep(0.4)
    chooser.value.set_files(file)
    time.sleep(0.8)
    return True


def clipboard_image_check(page, res: "H.Results", dashboard: str) -> None:
    """An image picked through Upload Image reaches the core as a blob, and
    anything else is refused.

    The upload button is the only way binary clipboard content leaves the
    client unasked, and Wish previews what was picked: a preview showing
    anything but the pixels of the picked file would be rendering something
    from somewhere else entirely.
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
    if not pick_clipboard_image(page, dashboard, {"name": "clip.png", "mimeType": "image/png",
                                                  "buffer": CLIPBOARD_PNG}):
        res.skip(f"{dashboard}: the clipboard image path", "no Upload Image button in the panel")
        return
    res.check(f"{dashboard}: a picked image reaches the core whole",
              page.evaluate("window.__clipImages") == [len(CLIPBOARD_PNG)],
              page.evaluate("window.__clipImages"))
    if dashboard == "wish":
        # The dialog closed the menu; the panel shows the pick again when reopened.
        page.keyboard.press("Escape")
        time.sleep(0.3)
        open_clipboard_panel(page, dashboard)
        time.sleep(0.4)
    # Nothing points at a URL for the picked file: the preview draws its pixels.
    urls = page.locator('img[src^="blob:"], img[src^="data:"]').count()
    drawn = page.evaluate("""() => {
      const canvas = document.querySelector('canvas[role="img"]');
      if (!canvas) return null;
      const pixel = canvas.getContext('2d').getImageData(0, 0, 1, 1).data;
      return { w: canvas.width, h: canvas.height, pixel: [...pixel] };
    }""")
    if dashboard == "wish":
        res.check("wish: the preview draws the picked image itself",
                  urls == 0 and drawn is not None and [drawn["w"], drawn["h"]] == [2, 2]
                  and drawn["pixel"][:3] == [255, 0, 0], drawn)
    else:
        res.check("classic: the panel shows no preview to point anywhere",
                  urls == 0 and drawn is None, drawn)
    if dashboard == "wish":
        for _ in range(2):
            page.keyboard.press("Escape")
            time.sleep(0.2)

    pick_clipboard_image(page, dashboard, {"name": "clip.txt", "mimeType": "text/plain",
                                           "buffer": b"not an image"})
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
    # The served document names the tab. Left to the client script, the browser
    # shows the host URL there until the bundle runs.
    shell = H.curl("/")[1].decode("utf-8", "replace")
    res.check("the served page names the tab", "<title>Selkies</title>" in shell,
              shell[:200])
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
            section = wish_gamepad_preview(page)
        res.check("gamepads section shown by default", section, section)

        if dashboard == "wish":
            wish_clipboard_seed_check(page, res)
            wish_stats_fit_check(page, res)
        gaming_mode_check(page, res, dashboard)
        if dashboard == "classic":
            classic_layout_check(page, res)
            classic_stats_strip_check(page, res)
        touch_gamepad_layer_check(page, res, dashboard)
        clipboard_image_check(page, res, dashboard)

        st = len(H.server_log())
        changed = False
        if dashboard == "classic":
            try:
                opened = classic_open_video(page)
                time.sleep(1.0)
                if page.locator('#framerateSlider').count():
                    # The slider carries an index into its frame rate stops, and the
                    # display's own stop sits right after the listed stop of its rate,
                    # so two stops down is a change of rate from whatever the default is.
                    page.evaluate("""() => {
                      const el = document.getElementById('framerateSlider');
                      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
                      setter.call(el, String(Number(el.value) > 1 ? Number(el.value) - 2 : 2));
                      el.dispatchEvent(new Event('input', {bubbles: true}));
                      el.dispatchEvent(new Event('change', {bubbles: true}));
                    }""")
                    changed = True
                else:
                    print("classic: framerateSlider not found after open:", opened)
            except Exception as e:
                print("classic slider err:", e)
        else:
            # The sliders are driven by click-to-focus plus arrow keys.
            try:
                opened = False
                trig = page.locator('button:has(svg.lucide-settings-2)').first
                if trig.count():
                    trig.click(force=True, timeout=3000)
                    time.sleep(1.2)
                    opened = page.locator('[data-slot="slider-thumb"]').count() > 0
                if not opened:
                    page.get_by_role("button", name=lambda n: "settings" in (n or "").lower()).first.click(force=True, timeout=2500)
                    time.sleep(1.2)
                    opened = page.locator('[data-slot="slider-thumb"]').count() > 0
                if opened:
                    track = page.locator('[data-slot="slider-thumb"]').first
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

        # The signaling handshake is refused (409, 503) while the server the
        # page streams from switches modes, and the page reconnects past it.
        real_errors = [e for e in console_errors if not any(
            bp in e for bp in ("Failed to load resource", "Unexpected server response:",
                               "Unexpected response code: 409", "Unexpected response code: 503",
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
    card in Wish) and nothing else, so the gamepad input toggle stays; and
    enable_resize=false offers no resolution to pick on the primary, whose
    size the server keeps."""
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
                       "SELKIES_ENABLE_RESIZE": "false",
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
            # The preview renders a titled visualizer ("Gamepad 0") in the same menu when shown.
            section = re.search(r"^Gamepad \d+$", menu, re.M) is not None
        res.check("core buttons reachable (microphone toggle present)", mic, mic)
        res.check("ui_sidebar_show_webcam=false hides webcam toggle", not cam, cam)
        res.check("ui_sidebar_show_gamepads=false keeps the gamepad input toggle", pad, pad)
        res.check("ui_sidebar_show_gamepads=false hides the gamepads section", not section, section)
        if dashboard == "classic":
            header = page.locator('.sidebar-section-header:has-text("Screen")').first
            header.scroll_into_view_if_needed()
            header.click()
            time.sleep(0.8)
            preset = page.locator('#resolutionPresetSelect').count()
        else:
            open_wish_settings_tab(page, "Resolution")
            preset = page.locator('button:has-text("Select Preset")').count()
        # The button beside the resolution controls, so an unopened panel cannot pass.
        scale = page.locator('button:has-text("Scale Locally")').count()
        res.check("screen settings reachable (scale-locally button present)", scale > 0, scale)
        res.check("enable_resize=false offers no resolution preset on the primary", preset == 0, preset)
        browser.close()
    res.summary()
    return res


def hidpi_default_block(dashboard: str, dist: str, mode: str = "websockets") -> "H.Results":
    """A deployment that configures a resolution turns HiDPI off, and the core
    has to stream that way. The dashboard resolves the default; the core starts
    from its own stored value, so without a push it streams pixel-perfect on
    every load while the toggle reads off. `useCssScaling` in the settings the
    core sends is what it is actually applying. Both cores resolve it, so both
    transports are driven."""
    res = H.Results(f"hidpi-{dashboard}-{mode}")
    H.server_start(mode=mode, wayland=False, web_root=dist,
                   extra_env={"SELKIES_MANUAL_WIDTH": "1280",
                              "SELKIES_MANUAL_HEIGHT": "800"})
    # The core persists every `useCssScaling` it applies, so the stored value is
    # what the session is running; the built-in default is the opposite, so a
    # stored "true" can only come from the configuration being honored.
    applied = """(() => {
      for (let i = 0; i < localStorage.length; i++) {
        const k = localStorage.key(i);
        if (k.endsWith('useCssScaling')) return localStorage.getItem(k);
      }
      return null;
    })()"""
    try:
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            ctx = browser.new_context(viewport={"width": 1440, "height": 900},
                                      device_scale_factor=2)
            ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            time.sleep(10.0)
            first = page.evaluate(applied)
            res.check("a configured resolution turns HiDPI off in the core",
                      first == "true", first)
            page.reload(wait_until="load")
            time.sleep(10.0)
            again = page.evaluate(applied)
            res.check("the reload comes back with HiDPI still off",
                      again == "true", again)
            browser.close()
    finally:
        H.server_stop()
    res.summary()
    return res


# Sends every server candidate the browser learns to a black hole and keeps the
# browser's own from the server, as a network that drops UDP both ways leaves
# the two agents: each session's checks go unanswered until the browser gives
# it up as failed, while the signaling socket, like the page, still reaches the
# server.
UDP_BLACKHOLE_JS = """
  (() => {
    const Orig = window.RTCPeerConnection;
    if (!Orig) return;
    const hole = (c) => c.replace(/(candidate:\\S+ \\d+ \\S+ \\d+ )\\S+( \\d+ typ)/, (m, head, tail) => head + '192.0.2.1' + tail);
    const setRemote = Orig.prototype.setRemoteDescription;
    Orig.prototype.setRemoteDescription = function(desc) {
      if (desc && desc.sdp) {
        desc = {type: desc.type, sdp: desc.sdp.split('\\r\\n').map((l) => l.startsWith('a=candidate:') ? hole(l) : l).join('\\r\\n')};
      }
      return setRemote.call(this, desc);
    };
    const addCandidate = Orig.prototype.addIceCandidate;
    Orig.prototype.addIceCandidate = function(c) {
      if (c && c.candidate) c = {candidate: hole(c.candidate), sdpMid: c.sdpMid, sdpMLineIndex: c.sdpMLineIndex};
      return addCandidate.call(this, c);
    };
    const handler = Object.getOwnPropertyDescriptor(Orig.prototype, 'onicecandidate');
    Object.defineProperty(Orig.prototype, 'onicecandidate', {
      configurable: true,
      get() { return handler.get.call(this); },
      set(fn) { handler.set.call(this, (e) => (e.candidate && e.candidate.candidate) ? undefined : fn(e)); },
    });
  })();
"""


def transport_advice_block(dashboard: str, dist: str, dual: bool, engine: str = "chromium") -> "H.Results":
    """A WebRTC session whose media path fails beside a working signaling
    socket raises a notice offering WebSockets, whichever engine gives the
    path up. With dual mode on, its button switches the server and the page
    comes back streaming over WebSockets; with it off, the notice has no
    button and says who can switch."""
    res = H.Results(f"transport-advice-{dashboard}-{'dual' if dual else 'single'}"
                    + ("" if engine == "chromium" else f"-{engine}"))
    H.server_start(mode="webrtc", wayland=False, web_root=dist,
                   extra_env={"SELKIES_ENABLE_DUAL_MODE": "true" if dual else "false"})
    if dashboard == "classic":
        notice, button = ".notification-item.transport", ".notification-transport-switch"
    else:
        notice = '[data-sonner-toast]:has-text("WebRTC cannot connect")'
        button = '[data-sonner-toast] button:has-text("Switch to WebSockets")'
    try:
        with sync_playwright() as p:
            browser = C.chromium_launch(p) if engine == "chromium" else C.launch_browser(p, engine)
            ctx = browser.new_context(viewport={"width": 1280, "height": 800})
            ctx.add_init_script(UDP_BLACKHOLE_JS)
            page = ctx.new_page()
            t0 = time.time()
            page.goto(H.BASE_URL, wait_until="load")
            try:
                page.wait_for_selector(notice, timeout=90000)
                shown = True
            except Exception:
                shown = False
            res.check("a failing media path raises the WebSockets notice", shown,
                      f"after {time.time() - t0:.1f}s")
            text = page.locator(notice).first.inner_text() if shown else ""
            has_button = shown and page.locator(button).count() > 0
            res.check("the notice has the switch exactly where the mode menu would",
                      shown and has_button == dual, f"button={has_button} dual={dual}")
            res.check("its text says what the reader can do",
                      ("administrator" in text) != dual, text.replace("\n", " | ")[:160])
            if dual and has_button:
                # The switch answers, then the core reloads the page into the new mode.
                with page.expect_navigation(wait_until="load", timeout=30000):
                    page.locator(button).first.click()
                ok = C.wait_ws_video(page, timeout=45) is not None
                mode = page.evaluate("window.__SELKIES_STREAMING_MODE__ || null")
                res.check("its switch brings the page back streaming over WebSockets",
                          ok and mode == "websockets", f"video={ok} mode={mode}")
            browser.close()
    finally:
        H.server_stop()
    res.summary()
    return res


# A preset the local density would never ask for, and the pick its own size
# derives: the shorter side against the 1080 rows 96 DPI is for.
DPI_PRESET = "2560x1440"
DPI_PRESET_PICK = 120


def wait_xft(want: int, timeout: float = 20) -> int:
    """The desktop's Xft.dpi, polled until it reads `want` or time runs out."""
    deadline = time.time() + timeout
    got = DA.xft_dpi()
    while time.time() < deadline and got != want:
        time.sleep(0.5)
        got = DA.xft_dpi()
    return got


def dpi_for_resolution_block(dashboard: str, dist: str) -> "H.Results":
    """A resolution picked in the dashboard carries the UI-scaling default with it.

    The picker and the desktop have to land on the same number: the framebuffer
    asked for is what the desktop's UI is sized from, the screen showing it
    says nothing about it, and the dashboard posts its own derived value over
    the core's whenever the two differ. Driven through the control a user uses,
    at dpr 1, where the display itself would ask for 96 whatever the resolution
    is. The classic dashboard's selects are what can be driven; the Wish
    dashboard mirrors the same formula, which
    tests/unit/test_dpi_default_mirror.py holds it to.
    """
    res = H.Results(f"dpi-for-resolution-{dashboard}")
    H.server_start(mode="websockets", wayland=False, web_root=dist)
    try:
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            ctx = browser.new_context(viewport={"width": 1000, "height": 700},
                                      device_scale_factor=1)
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            res.check("video flowing", bool(wait_canvas(page, 30)), "")
            page.locator('.toggle-handle').first.click()
            time.sleep(0.8)
            header = page.locator('.sidebar-section-header:has-text("Screen")').first
            header.scroll_into_view_if_needed()
            header.click()
            time.sleep(0.8)
            # The desktop's own DPI is left where the last session put it until
            # something changes it, so the picker is where the starting pick
            # reads: the display's, with no resolution of its own to follow.
            before = page.locator("#uiScalingSelect").input_value()
            res.check("the picker starts on the display's own pick", before == "96", before)
            page.select_option("#resolutionPresetSelect", DPI_PRESET)
            time.sleep(1.0)
            dpi = wait_xft(DPI_PRESET_PICK)
            res.check(f"a {DPI_PRESET} preset scales the desktop to its own pick",
                      dpi == DPI_PRESET_PICK, f"Xft.dpi={dpi}")
            shown = page.locator("#uiScalingSelect").input_value()
            res.check("and the picker shows the same number",
                      shown == str(DPI_PRESET_PICK), shown)
            browser.close()
    finally:
        H.server_stop()
    res.summary()
    return res


def second_screen_block(dashboard: str, dist: str) -> "H.Results":
    """A refused second-display window leaves the placement arrows up.

    The window is opened from an async continuation, so the click's transient
    activation may be spent by then and the browser refuses it. The page is
    given a `window.open` that refuses every one, which is what a popup blocker
    does (the headless shell honors a switch for it, a full Chrome in its
    new headless mode does not): clearing the arrows on a window that never opened is the button
    appearing to do nothing at all.
    """
    res = H.Results(f"second-screen-{dashboard}")
    H.server_start(mode="websockets", wayland=False, web_root=dist)
    arrows = "[...document.querySelectorAll('.screen-placement-overlay button')]"
    try:
        with sync_playwright() as p:
            browser = C.chromium_launch(p, extra_args=["--block-new-web-contents"])
            ctx = browser.new_context(viewport={"width": 1440, "height": 900})
            ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';"
                                "window.open = () => null;")
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            time.sleep(8.0)
            if dashboard == "wish":
                opened = wish_open_menu_item(page, "Add a second screen")
            else:
                opened = classic_open_video(page)
                page.evaluate("""() => {
                  const header = [...document.querySelectorAll('.sidebar-section-header')]
                    .find(e => /screen/i.test(e.textContent));
                  if (header) header.click();
                }""")
                time.sleep(1.0)
                page.evaluate("""() => {
                  const add = [...document.querySelectorAll('button')]
                    .find(b => /add screen/i.test(b.textContent));
                  if (add) add.click();
                }""")
            time.sleep(2.0)
            shown = page.evaluate(f"{arrows}.length")
            res.check("the button offers a placement when it cannot choose one",
                      shown > 0, f"{shown} arrows, menu reached={opened}")
            page.evaluate(f"{arrows}[0] && {arrows}[0].click()")
            time.sleep(1.5)
            still = page.evaluate(f"{arrows}.length")
            res.check("a refused window leaves the arrows up rather than clearing them",
                      still == shown, f"{still} of {shown} arrows")
            res.check("no second window was opened", len(ctx.pages) == 1,
                      f"{len(ctx.pages)} page(s)")
            if dashboard == "classic":
                classic_arrow_theme_check(page, res)
            browser.close()
    finally:
        H.server_stop()
    res.summary()
    return res


def classic_arrow_theme_check(page, res: "H.Results") -> None:
    """The placement arrows are the theme's filled button in either theme.

    The overlay is a sibling of the sidebar, so it only sees the palette it
    carries the theme class for itself; a stale color here is the one
    control on the page that never follows the theme.
    """
    probe = """() => {
      const arrow = document.querySelector('.screen-placement-overlay button');
      const swatch = document.createElement('div');
      swatch.style.background = 'var(--button-bg)';
      document.querySelector('.sidebar').appendChild(swatch);
      const out = {arrow: getComputedStyle(arrow).backgroundColor,
                   token: getComputedStyle(swatch).backgroundColor};
      swatch.remove();
      return out;
    }"""
    seen = {}
    for flip in (False, True):
        if flip:
            page.evaluate("document.querySelector('.theme-toggle').click()")
            time.sleep(0.5)
        theme = page.evaluate(
            "document.querySelector('.sidebar').className.includes('theme-light') ? 'light' : 'dark'")
        colors = page.evaluate(probe)
        seen[theme] = colors["arrow"]
        res.check(f"placement arrows use the {theme} theme's button fill",
                  colors["arrow"] == colors["token"], f"{colors}")
    res.check("the arrow fill follows a theme flip",
              len(seen) == 2 and len(set(seen.values())) == 2, f"{seen}")


def second_screen_auto_block(dashboard: str, dist: str) -> "H.Results":
    """With one adjacent screen the button places the display on it, unasked.

    The arrows exist for every other case; this is the branch that skips them,
    and it needs a browser that reports two screens. Chromium is given a faked
    screen list and the window-management permission over CDP (a persistent
    context, which is where a browser-level grant lands), since the Window
    Management API answers nothing without it.
    """
    res = H.Results(f"second-screen-auto-{dashboard}")
    H.server_start(mode="websockets", wayland=False, web_root=dist)
    profile = tempfile.mkdtemp(prefix=f"selkies-screens-{dashboard}-")
    try:
        with sync_playwright() as p:
            ctx = p.chromium.launch_persistent_context(
                profile, headless=True,
                args=C.BROWSER_ARGS + ["--screen-info={0,0 1280x1024}{1280,0 1280x1024}"],
                viewport={"width": 1280, "height": 900})
            page = ctx.pages[0] if ctx.pages else ctx.new_page()
            ctx.new_cdp_session(page).send(
                "Browser.grantPermissions",
                {"origin": H.BASE_URL, "permissions": ["windowManagement"]})
            page.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
            page.goto(H.BASE_URL, wait_until="load")
            time.sleep(8.0)
            res.check("the browser reports the second screen",
                      page.evaluate("() => window.screen.isExtended") is True)

            opened = []
            ctx.on("page", lambda pg: opened.append(pg))
            # Clicked through the browser, not evaluate(): opening a window needs
            # the transient activation only a real gesture carries.
            if dashboard == "wish":
                wish_open_menu_item(page, "Add a second screen")
            else:
                classic_open_video(page)
                page.locator('.sidebar-section-header:has-text("Screen")').first.click()
                time.sleep(1.0)
                page.get_by_role("button", name=re.compile("add screen", re.I)).first.click()
            time.sleep(4.0)
            arrows = page.evaluate(
                "[...document.querySelectorAll('.screen-placement-overlay button')].length")
            res.check("it places the display without asking which side",
                      arrows == 0 and len(opened) == 1, f"{arrows} arrows, {len(opened)} window(s)")
            if opened:
                opened[0].wait_for_load_state("load")
                res.check("on the side the adjacent screen is",
                          opened[0].url.endswith("#display2-right"), opened[0].url[-24:])
                time.sleep(8.0)
                displays = json.loads(H.curl("/api/status")[1] or "{}")
                res.check("and the server takes the second display",
                          wait_second_display(), str(displays.get("current_mode")))
            ctx.close()
    finally:
        H.server_stop()
    res.summary()
    return res


def open_wish_settings_tab(page, tab: str) -> bool:
    """Open the Wish Settings panel (the Settings2 icon in the control strip)
    and switch it to the tab labeled `tab`."""
    trig = page.locator('button:has(svg.lucide-settings-2)').first
    if not trig.count():
        return False
    trig.click(force=True, timeout=3000)
    time.sleep(1.0)
    tab_button = page.locator(f'[role="tab"]:has-text("{tab}")').first
    if not tab_button.count():
        return False
    tab_button.click()
    time.sleep(0.8)
    return True


def click_raw_pointer_motion(page, dashboard: str) -> bool:
    """Click the raw pointer motion toggle in either dashboard's screen settings.

    Returns:
        True when the toggle was found and clicked.
    """
    if dashboard == "classic":
        classic_open_video(page)
        header = page.locator('.sidebar-section-header:has-text("Screen")').first
        if not header.count():
            return False
        header.click()
        time.sleep(1.0)
        toggle = page.locator('#rawPointerMotionToggle')
    else:
        if not open_wish_settings_tab(page, "Resolution"):
            return False
        toggle = page.locator(
            'div:has(> div > label:has-text("Raw pointer motion")) > [role="switch"]')
    if not toggle.count():
        return False
    toggle.first.scroll_into_view_if_needed()
    toggle.first.click()
    return True


def raw_pointer_motion_block(dashboard: str, dist: str, mode: str = "websockets") -> "H.Results":
    """The raw pointer motion toggle reaches the running client and is kept.

    On this platform the setting resolves on (the server's default, and Linux
    is not the platform the client turns it off for), so the toggle starts on;
    a click turns it off in the live `Input`, the core persists the pick, and a
    reload comes back off against the server's default of on. Driven on both
    transports, since each core applies the setting itself.
    """
    res = H.Results(f"raw-motion-{dashboard}-{mode}")
    H.server_start(mode=mode, wayland=False, web_root=dist)
    state = "() => window.webrtcInput ? window.webrtcInput.constructor.rawPointerMotion : null"
    stored = """(() => {
      for (let i = 0; i < localStorage.length; i++) {
        const k = localStorage.key(i);
        if (k.endsWith('_raw_pointer_motion')) return localStorage.getItem(k);
      }
      return null;
    })()"""
    try:
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            ctx = browser.new_context(viewport={"width": 1440, "height": 900})
            ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            time.sleep(8.0)
            res.check("the client starts with raw pointer motion on, the server's default",
                      page.evaluate(state) is True, page.evaluate(state))
            clicked = click_raw_pointer_motion(page, dashboard)
            res.check("the toggle is offered in the screen settings", clicked, clicked)
            time.sleep(1.5)
            res.check("a click turns raw pointer motion off in the running client",
                      page.evaluate(state) is False, page.evaluate(state))
            res.check("the core persists the pick", page.evaluate(stored) == "false",
                      page.evaluate(stored))
            page.reload(wait_until="load")
            time.sleep(8.0)
            res.check("a reload keeps the pick over the server default",
                      page.evaluate(state) is False, page.evaluate(state))
            C.close_browser(browser)
    finally:
        H.server_stop()
    res.summary()
    return res


def pick_rate_control(page, dashboard: str, mode: str) -> bool:
    """Pick `mode` ("cbr" or "crf") in either dashboard's rate-control menu,
    opening the video settings first where they are closed.

    Returns:
        True when the menu was found and the pick made.
    """
    try:
        if dashboard == "classic":
            if not page.locator('#rateControlSelect').count():
                classic_open_video(page)
            page.select_option('#rateControlSelect', mode, timeout=5000)
            return True
        box = page.locator("div.space-y-2:has(> label:text-is('Encoder Rate Control Mode'))")
        if not box.count():
            open_wish_settings_tab(page, "Video")
        box.first.locator("button").first.click(timeout=5000)
        item = "CRF (Constant Quality)" if mode == "crf" else "CBR (Constant Bitrate)"
        page.locator(f"[role='menuitem']:has-text('{item}')").first.click(timeout=5000)
        return True
    except Exception:
        return False


def rate_control_block(dashboard: str, dist: str, mode: str = "websockets") -> "H.Results":
    """A rate-control pick streams at the quality the dashboard shows.

    The operator sets a CRF and a bitrate of its own. Picking CRF, then CBR
    again, the dashboard shows the operator's value for the mode picked, and
    the stream has to run at it: the server never moves off it, and the capture
    the switch restarts comes up at it. Over WebRTC the core restates the value
    the new mode reads after the mode itself, so a value of the core's own
    would move the stream off what the page shows.
    """
    res = H.Results(f"rate-control-{dashboard}-{mode}")
    H.server_start(mode=mode, wayland=False, web_root=dist,
                   extra_env={"SELKIES_VIDEO_CRF": "30", "SELKIES_VIDEO_BITRATE": "4000"})
    steps = (
        {"pick": "crf", "label": "Video CRF (", "shown": "(30)", "line": "CRF: 30",
         "moved": r"Updated CRF live: \d+ -> (\d+)", "value": "30"},
        {"pick": "cbr", "label": "Video Bitrate (", "shown": "(4 Mbps)", "line": "CBR 4000",
         "moved": r"Updated video bitrate: \d+ -> (\d+)", "value": "4000"},
    )
    try:
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            ctx = browser.new_context(viewport={"width": 1440, "height": 900})
            ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            if mode == "webrtc":
                C.wait_wr_video(page, timeout=60)
            else:
                C.wait_ws_video(page, timeout=45)
            time.sleep(2.0)
            for step in steps:
                pick = step["pick"]
                mark = len(H.server_log())
                res.check(f"{pick}: the menu takes the pick", pick_rate_control(page, dashboard, pick), "")
                deadline = time.time() + 15
                while time.time() < deadline and "Stream settings active" not in H.server_log()[mark:]:
                    time.sleep(0.5)
                time.sleep(2.0)
                log = H.server_log()[mark:]
                lines = [ln for ln in log.splitlines() if "Stream settings active" in ln]
                text = page.locator("label", has_text=step["label"]).first.inner_text(timeout=3000)
                res.check(f"{pick}: the dashboard shows the operator's value", step["shown"] in text, text)
                targets = re.findall(step["moved"], log)
                res.check(f"{pick}: the server keeps the value the dashboard shows",
                          all(t == step["value"] for t in targets), targets)
                res.check(f"{pick}: the restarted capture runs at it",
                          bool(lines) and step["line"] in lines[-1], lines[-1:] or "no stream line")
            C.close_browser(browser)
    finally:
        H.server_stop()
    res.summary()
    return res


def paint_over_switches(page, dashboard: str):
    """The Turbo and paint-over switches of either dashboard, opening its video
    settings first."""
    if dashboard == "classic":
        if not page.locator('#videoStreamingModeToggle').count():
            classic_open_video(page)
        return page.locator('#videoStreamingModeToggle'), page.locator('#usePaintOverQualityToggle')
    if not page.locator("label:text-is('Use Paint-Overs')").count():
        open_wish_settings_tab(page, "Video")

    def row(label: str):
        return page.locator(f"div.justify-between:has(> div > label:text-is('{label}')) [role='switch']").first
    return row("Turbo"), row("Use Paint-Overs")


def switch_on(switch) -> bool:
    """Whether a dashboard switch reads on (the classic toggle's aria-pressed, the wish Switch's aria-checked)."""
    return (switch.get_attribute("aria-pressed") or switch.get_attribute("aria-checked")) == "true"


def paint_over_block(dashboard: str, dist: str, engine: str) -> "H.Results":
    """Paint-over follows Turbo where nothing pins it.

    A video encoder under Turbo sends every frame and leaves no still screen to
    clean up, so the dashboard shows paint-over off under Turbo and on without
    it, and sends the server each value it derives. A pick the user makes, or
    an operator's value, holds through a Turbo change.
    """
    res = H.Results(f"paint-over-{dashboard}-{engine}")
    for operator in (None, "true"):
        who = "operator on" if operator else "unset"
        H.server_start(mode="websockets", wayland=False, web_root=dist,
                       extra_env={"SELKIES_USE_PAINT_OVER_QUALITY": operator} if operator else {})
        try:
            with sync_playwright() as p:
                browser = C.launch_browser(p, engine)
                ctx = browser.new_context(viewport={"width": 1440, "height": 900})
                ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
                # The settings the core sends. Playwright reports the worker socket's frames
                # in Chromium and WebKit but no WebSocket frames at all in Firefox, whose
                # socket therefore stays on the page (`socket_worker=false`), where its sends
                # are recorded.
                ctx.add_init_script("""(() => {
                    const send = WebSocket.prototype.send;
                    window.__settingsSent = [];
                    WebSocket.prototype.send = function (data) {
                        if (typeof data === 'string' && data.startsWith('SETTINGS,')) window.__settingsSent.push(data);
                        return send.call(this, data);
                    };
                })();""")
                page = ctx.new_page()
                frames = []
                page.on("websocket", lambda ws, frames=frames: ws.on(
                    "framesent", lambda f: frames.append(f) if isinstance(f, str) else None))

                def pushed(page=page, frames=frames):
                    # One record or the other: Playwright's own Firefox reports the frames the
                    # page records as well, and counting both doubles every push.
                    sent = frames or page.evaluate("window.__settingsSent || []")
                    return [json.loads(m[len("SETTINGS,"):]).get("use_paint_over_quality") for m in sent
                            if m.startswith("SETTINGS,") and "use_paint_over_quality" in m]

                def flip(switch):
                    switch.click(timeout=5000)
                    time.sleep(2.0)

                page.goto(f"{H.BASE_URL}?socket_worker=false" if engine == "firefox" else H.BASE_URL,
                          wait_until="load")
                C.wait_ws_video(page, timeout=45)
                time.sleep(2.0)
                turbo, paint = paint_over_switches(page, dashboard)
                res.check(f"{who}: Turbo starts on", switch_on(turbo), "")
                if operator:
                    res.check(f"{who}: paint-over shows the operator's value under Turbo", switch_on(paint), "")
                    flip(turbo)
                    flip(turbo)
                    res.check(f"{who}: a Turbo change leaves it on", switch_on(paint), "")
                    res.check(f"{who}: and never sends it off", False not in pushed(), pushed())
                else:
                    res.check(f"{who}: paint-over defaults off under Turbo", not switch_on(paint), "")
                    res.check(f"{who}: and the server is sent off", pushed()[-1:] == [False], pushed())
                    flip(turbo)
                    res.check(f"{who}: turning Turbo off turns paint-over on", switch_on(paint), "")
                    res.check(f"{who}: and sends it on", pushed()[-1:] == [True], pushed())
                    flip(turbo)
                    res.check(f"{who}: turning Turbo on turns it off again", not switch_on(paint), "")
                    res.check(f"{who}: and sends it off", pushed()[-1:] == [False], pushed())
                    flip(paint)
                    mark = len(pushed())
                    flip(turbo)
                    flip(turbo)
                    res.check(f"{who}: a user's pick holds through a Turbo change", switch_on(paint), "")
                    res.check(f"{who}: and nothing sends it off after", False not in pushed()[mark:], pushed())
                C.close_browser(browser)
        finally:
            H.server_stop()
    res.summary()
    return res


def wait_second_display(timeout: float = 15.0) -> bool:
    """Whether the server logs a second display client joining."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if "display2" in H.server_log():
            return True
        time.sleep(0.5)
    return False


def page_socket_block(dashboard: str, dist: str, engine: str) -> "H.Results":
    """With the session socket on the page (`socket_worker=false`, or a policy that forbids
    blob workers), the settings the server sends at connect can arrive before the dashboard's
    listeners are up, WebKit's first among them; the dashboard still starts from them, so the
    controls they gate come up enabled."""
    res = H.Results(f"page-socket-{dashboard}-{engine}")
    H.server_start(mode="websockets", wayland=False, web_root=dist)
    try:
        with sync_playwright() as p:
            browser = C.launch_browser(p, engine)
            ctx = browser.new_context(viewport={"width": 1440, "height": 900})
            ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
            page = ctx.new_page()
            page.goto(f"{H.BASE_URL}?socket_worker=false", wait_until="load")
            C.wait_ws_video(page, timeout=45)
            time.sleep(2.0)
            turbo, _ = paint_over_switches(page, dashboard)
            res.check("the settings the server sent reach the dashboard", turbo.is_enabled(),
                      f"Turbo switch disabled={turbo.get_attribute('disabled')}")
            C.close_browser(browser)
    finally:
        H.server_stop()
    res.summary()
    return res


# The languages the dashboards ship, by the locale a browser would ask for.
LOCALES = ["en-US", "es-ES", "zh-CN", "hi-IN", "pt-BR", "fr-FR", "ru-RU", "de-DE", "tr-TR", "it-IT",
           "nl-NL", "ar-SA", "ko-KR", "ja-JP", "vi-VN", "th-TH", "fil-PH", "da-DK", "zh-TW"]
# What of the open sidebar reaches past its right edge, or is cut off inside a box.
SPILL = """() => {
  const bar = document.querySelector('.sidebar.is-open');
  if (!bar) return null;
  const edge = bar.getBoundingClientRect().right;
  const out = bar.scrollWidth > bar.clientWidth + 1 ? ['sidebar scrolls sideways'] : [];
  for (const el of bar.querySelectorAll('*')) {
    const r = el.getBoundingClientRect();
    if (!r.width || !r.height) continue;
    const clipped = getComputedStyle(el).overflowX !== 'visible' && el.scrollWidth > el.clientWidth + 1;
    if (r.right > edge + 1 || clipped) out.push(`${el.className || el.tagName} ${(el.innerText || '').trim().slice(0, 30)}`);
  }
  return out.slice(0, 5);
}"""
PHONE = {"width": 320, "height": 568}


def layout_block() -> "H.Results":
    """The classic sidebar, every section open and on a touch screen the key
    palette too, keeps its contents inside it in every language the dashboards
    ship, on a desktop window and a 320-pixel phone; the wish dashboard's top
    bar and soft keys fit that phone."""
    res = H.Results("dash-layout")
    H.server_start(mode="websockets", wayland=False, web_root=H.CLASSIC_DIST)
    try:
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            for form, viewport, touch in (("desktop", {"width": 1280, "height": 900}, False), ("phone", PHONE, True)):
                spilled = {}
                for locale in LOCALES:
                    ctx = browser.new_context(viewport=viewport, locale=locale, has_touch=touch, is_mobile=touch)
                    ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
                    page = ctx.new_page()
                    page.goto(H.BASE_URL, wait_until="load")
                    wait_chunk(page, 40)
                    if not page.evaluate("!!document.querySelector('.sidebar.is-open')"):
                        page.locator('.toggle-handle').first.click(force=True)
                        time.sleep(0.8)
                    heads = page.locator('.sidebar-section-header')
                    for i in range(heads.count()):
                        if heads.nth(i).get_attribute("aria-expanded") != "true":
                            heads.nth(i).click(force=True)
                    palette = page.locator('.key-palette-toggle').first
                    if palette.count() and palette.is_visible():
                        palette.click(force=True)
                    time.sleep(0.6)
                    found = page.evaluate(SPILL)
                    if found is None or found:
                        spilled[locale] = found
                    ctx.close()
                res.check(f"classic: the sidebar holds its contents in every language ({form})", not spilled, spilled)
            browser.close()
    finally:
        H.server_stop()
    H.server_start(mode="websockets", wayland=False, web_root=H.WISH_DIST)
    try:
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            ctx = browser.new_context(viewport=PHONE, has_touch=True, is_mobile=True)
            ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            wait_chunk(page, 40)
            time.sleep(1.0)
            reach = page.evaluate("""() => Math.max(...[...document.querySelectorAll('#dashboard-root button')]
              .filter((b) => b.offsetParent).map((b) => b.getBoundingClientRect().right))""")
            res.check("wish: the top bar and the soft keys fit a 320-pixel phone", reach <= PHONE["width"], reach)
            browser.close()
    finally:
        H.server_stop()
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
    if which in ("all", "hidpi"):
        blocks.append(hidpi_default_block("classic", H.CLASSIC_DIST))
        blocks.append(hidpi_default_block("wish", H.WISH_DIST))
    if which in ("all", "hidpi-webrtc"):
        blocks.append(hidpi_default_block("classic", H.CLASSIC_DIST, "webrtc"))
        blocks.append(hidpi_default_block("wish", H.WISH_DIST, "webrtc"))
    if which in ("all", "raw-motion"):
        blocks.append(raw_pointer_motion_block("classic", H.CLASSIC_DIST))
        blocks.append(raw_pointer_motion_block("wish", H.WISH_DIST))
    if which in ("all", "raw-motion-webrtc"):
        blocks.append(raw_pointer_motion_block("classic", H.CLASSIC_DIST, "webrtc"))
        blocks.append(raw_pointer_motion_block("wish", H.WISH_DIST, "webrtc"))
    if which in ("all", "rate-control"):
        blocks.append(rate_control_block("classic", H.CLASSIC_DIST))
        blocks.append(rate_control_block("wish", H.WISH_DIST))
    if which in ("all", "rate-control-webrtc"):
        blocks.append(rate_control_block("classic", H.CLASSIC_DIST, "webrtc"))
        blocks.append(rate_control_block("wish", H.WISH_DIST, "webrtc"))
    if which in ("all", "paint-over"):
        for engine in ("chromium", "firefox", "webkit"):
            blocks.append(paint_over_block("classic", H.CLASSIC_DIST, engine))
            blocks.append(paint_over_block("wish", H.WISH_DIST, engine))
    if which in ("all", "page-socket"):
        for engine in ("chromium", "firefox", "webkit"):
            blocks.append(page_socket_block("classic", H.CLASSIC_DIST, engine))
            blocks.append(page_socket_block("wish", H.WISH_DIST, engine))
    if which in ("all", "dpi-resolution"):
        blocks.append(dpi_for_resolution_block("classic", H.CLASSIC_DIST))
    if which in ("all", "transport-advice"):
        for dual in (True, False):
            blocks.append(transport_advice_block("classic", H.CLASSIC_DIST, dual))
            blocks.append(transport_advice_block("wish", H.WISH_DIST, dual))
        for engine in ("firefox", "webkit"):
            blocks.append(transport_advice_block("classic", H.CLASSIC_DIST, True, engine))
    if which in ("all", "layout"):
        blocks.append(layout_block())
    if which in ("all", "second-screen"):
        blocks.append(second_screen_block("classic", H.CLASSIC_DIST))
        blocks.append(second_screen_block("wish", H.WISH_DIST))
        blocks.append(second_screen_auto_block("classic", H.CLASSIC_DIST))
        blocks.append(second_screen_auto_block("wish", H.WISH_DIST))
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== DASH: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
