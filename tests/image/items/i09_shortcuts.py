"""9. Test all of the shortcuts, and especially the pointer lock when `Ctrl + Shift + Left Click` or full screen is used.

The client's chords, pressed on the page: Ctrl+Shift+M opens and closes the
dashboard, Ctrl+Shift+G shows and hides the touch gamepad, Ctrl+Shift+F enters
fullscreen, Ctrl+Shift+Left Click locks the pointer to the stream, and
Ctrl+Shift+X turns gaming mode on, which goes fullscreen and locks the
pointer there. With shortcuts turned off the chords are the session's.
Headless WebKit has neither fullscreen nor pointer lock to grant.
"""
import time
from typing import Any

from image_lib import open_sidebar, reveal

ITEM = 9
TITLE = "shortcuts and pointer lock"


def chord(page: Any, key: str) -> None:
    page.keyboard.down("Control")
    page.keyboard.down("Shift")
    page.keyboard.press(key)
    page.keyboard.up("Shift")
    page.keyboard.up("Control")
    time.sleep(1.2)


def state(page: Any) -> dict:
    return page.evaluate("""() => ({
      sidebar: !!document.querySelector('.sidebar.is-open'),
      fullscreen: !!document.fullscreenElement,
      lock: !!document.pointerLockElement,
      pad: [...document.querySelectorAll('#universal-touch-gamepad-controls-overlay')]
             .some(e => getComputedStyle(e).display !== 'none' && e.children.length > 0),
      gaming: !!(window.__it && window.__it.gaming),
    })""")


def leave(page: Any) -> None:
    page.evaluate("document.pointerLockElement && document.exitPointerLock()")
    page.evaluate("document.fullscreenElement && document.exitFullscreen()")
    time.sleep(1.0)


def run(cell: Any) -> None:
    R = cell.res
    page = cell.open()
    page.mouse.click(900, 600)
    time.sleep(0.5)
    a = state(page)
    chord(page, "KeyM")
    b = state(page)
    chord(page, "KeyM")
    c = state(page)
    R.check("Ctrl+Shift+M opens and closes the dashboard", a["sidebar"] != b["sidebar"] and c["sidebar"] == a["sidebar"],
            f"{a['sidebar']} -> {b['sidebar']} -> {c['sidebar']}")
    chord(page, "KeyG")
    b = state(page)
    chord(page, "KeyG")
    c = state(page)
    R.check("Ctrl+Shift+G shows and hides the touch gamepad", b["pad"] and not c["pad"], f"{b['pad']} -> {c['pad']}")
    if cell.engine == "webkit":
        R.skip("fullscreen, pointer lock, gaming mode", "headless WebKit grants neither fullscreen nor pointer lock")
    else:
        chord(page, "KeyF")
        f = state(page)
        R.check("Ctrl+Shift+F enters fullscreen", f["fullscreen"], f)
        page.keyboard.press("Escape")
        time.sleep(1.0)
        leave(page)
        page.keyboard.down("Control")
        page.keyboard.down("Shift")
        page.mouse.click(700, 400)
        page.keyboard.up("Shift")
        page.keyboard.up("Control")
        time.sleep(1.2)
        lk = state(page)
        R.check("Ctrl+Shift+Left Click locks the pointer to the stream", lk["lock"], lk)
        leave(page)
        chord(page, "KeyX")
        g = state(page)
        if g["gaming"] and not g["lock"]:
            page.mouse.click(700, 400)
            time.sleep(1.5)
            g = state(page)
        R.check("Ctrl+Shift+X turns gaming mode on: fullscreen with the pointer locked",
                g["gaming"] and g["fullscreen"] and g["lock"], g)
        chord(page, "KeyX")
        leave(page)
        R.check("Ctrl+Shift+X turns gaming mode off again", not state(page)["gaming"], state(page))
    # Shortcuts off: the chords are the session's.
    if reveal(page, "#keyboardShortcutsToggle"):
        page.locator("#keyboardShortcutsToggle").first.click(timeout=5000)
        time.sleep(1.0)
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.8)
        before = state(page)["sidebar"]
        chord(page, "KeyM")
        after = state(page)["sidebar"]
        R.check("with shortcuts off, Ctrl+Shift+M no longer opens the dashboard", after == before, f"{before} -> {after}")
        open_sidebar(page)
        if reveal(page, "#keyboardShortcutsToggle"):
            page.locator("#keyboardShortcutsToggle").first.click(timeout=5000)
    else:
        R.skip("shortcuts off", "no shortcuts switch in the dashboard")
