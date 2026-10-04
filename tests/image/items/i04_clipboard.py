"""4. Use very large text clipboard payloads (>2MB) and image payloads. Check if the Image Upload (clipboard) button works as well.

With Chrome's focus emulation off and the client headed on its own X display,
the client's clipboard is the real X one, read and written there with xclip as
a local application would. Both ways, for 2.5 MB of text and a multi-MB PNG:
the session's side is xclip on the session's display (an application copying
or pasting there). The page must hold focus for the browser to touch the
clipboard at all, and is clicked first, as a user does; it is also left and
re-entered, which is when Chromium reads a local copy. Upload Image goes
through the clipboard panel's own button and file chooser.

Headless WebKit has no X clipboard: there only what reaches the page is
checked, and Upload Image.
"""
import hashlib
import struct
import time
from typing import Any, Optional

from image_lib import H, open_sidebar, session_browser, stop_session_browser, TD
from test_clipboard_image import noise_png

ITEM = 4
TITLE = "clipboard: >2 MB text, multi-MB images, Upload Image"
CLASS = {"chromium": "google-chrome|chromium|Chromium", "firefox": "firefox|Firefox"}
TMP = "/tmp/selkies-imagetest"


def dims(head: bytes) -> Optional[tuple]:
    """A PNG's width and height from its first 24 bytes, or None for anything else."""
    if len(head) >= 24 and head[:8] == b"\x89PNG\r\n\x1a\n":
        return struct.unpack(">II", head[16:24])
    return None


def big_text(tag: str) -> bytes:
    line = (f"selkies image tier {tag} " + "0123456789abcdef" * 4 + "\n").encode()
    return (line * (2600 * 1024 // len(line) + 1))[:2600 * 1024]


class Focus:
    """Real X focus on the client display: the browser's window, or a plain one of our own."""

    def __init__(self, cell: Any) -> None:
        self.cell, self.other = cell, None

    def window(self) -> Optional[str]:
        out = self.cell.xclient("xdotool", "search", "--onlyvisible", "--class", CLASS[self.cell.engine]).stdout.split()
        return out[0].decode() if out else None

    def enter(self, page: Any) -> bool:
        wid = self.window()
        if wid:
            self.cell.xclient("xdotool", "windowfocus", "--sync", wid)
        page.bring_to_front()
        # A click on the stream, as a user's first one: it lands on the session's pattern page.
        page.mouse.click(900, 600)
        time.sleep(0.5)
        return page.evaluate("document.hasFocus()")

    def leave(self) -> None:
        from selkies.Xlib import X, display
        if not self.other:
            d = display.Display(H.require_display())
            w = d.screen().root.create_window(1200, 0, 60, 40, 0, d.screen().root_depth, X.InputOutput,
                                              X.CopyFromParent, background_pixel=d.screen().white_pixel,
                                              override_redirect=True)
            w.map()
            d.sync()
            self.other = (d, w)
        d, w = self.other
        w.set_input_focus(X.RevertToParent, X.CurrentTime)
        d.sync()
        time.sleep(0.4)


class XclipSession:
    """The session's side as an X application: xclip owns and reads the session's clipboard."""

    def __init__(self, cell: Any) -> None:
        self.cell = cell

    def prepare(self, page: Any, text: bytes, image: bytes) -> bool:
        return self.cell.target.put(f"{TMP}/clip.txt", text) and self.cell.target.put(f"{TMP}/clip.png", image)

    def copy(self, page: Any, kind: str) -> None:
        path, mime = (f"{TMP}/clip.txt", "UTF8_STRING") if kind == "text" else (f"{TMP}/clip.png", "image/png")
        self.cell.target.sh(f"(setsid xclip -selection clipboard -t {mime} -i {path} > /dev/null 2>&1 &)")

    def read(self, page: Any, kind: str) -> tuple:
        """`(size, sha256 or PNG dims)` of the session clipboard's text or image."""
        mime = "UTF8_STRING" if kind == "text" else "image/png"
        out = self.cell.target.out(f"f={TMP}/clip-read; timeout 20 xclip -selection clipboard -t {mime} -o > $f "
                                   "2>/dev/null; stat -c %s $f; sha256sum < $f | cut -c1-64; "
                                   "head -c 24 $f | od -An -tx1 | tr -d ' \\n'")
        parts = out.split()
        if len(parts) < 2:
            return (0, None)
        return (int(parts[0]), parts[1] if kind == "text" else dims(bytes.fromhex(parts[2]) if len(parts) > 2 else b""))

    def done(self) -> None:
        self.cell.target.sh(f"rm -f {TMP}/clip.txt {TMP}/clip.png {TMP}/clip-read")


class PageSession:
    """The session's side as a page in the session's own Chrome: it copies on a key
    and reports what a paste brings, which is the Wayland desktop's clipboard as
    its applications see it."""

    def __init__(self, cell: Any) -> None:
        self.cell = cell

    def prepare(self, page: Any, text: bytes, image: bytes) -> bool:
        t = self.cell.target
        t.put(f"{TMP}/site/clip.txt", text)
        t.put(f"{TMP}/site/clip.png", image)
        self.cell.clear_report("clip")
        session_browser(t, "chrome", "/clip.html", kiosk=True, profile="clip")
        return bool(self.cell.report("clip", 30, where=lambda r: r.get("ready")))

    def copy(self, page: Any, kind: str) -> None:
        # The click focuses the session's page first, as a user's would before a key:
        # a key right behind it can reach the session before its window has focus.
        page.mouse.click(1100, 650)
        time.sleep(0.5)
        page.keyboard.press("t" if kind == "text" else "i")

    def read(self, page: Any, kind: str) -> tuple:
        before = len((self.cell.report("clip", 2) or {}).get("pasted", []))
        page.mouse.click(1100, 650)
        time.sleep(0.5)
        page.keyboard.press("Control+v")
        rep = self.cell.report("clip", 20, where=lambda r: len(r.get("pasted", [])) > before)
        last = (rep or {}).get("pasted", [{}])[-1] if rep else {}
        if kind == "text":
            t = last.get("text") or {}
            return (t.get("size", 0), t.get("sha256"))
        i = last.get("image") or {}
        return (i.get("size", 0), (i["w"], i["h"]) if "w" in i else None)

    def done(self) -> None:
        stop_session_browser(self.cell.target, "chrome", "clip")
        self.cell.target.sh(f"rm -f {TMP}/site/clip.txt {TMP}/site/clip.png")


def client_own(cell: Any, data: bytes, mime: str) -> None:
    cell.xclient("xclip", "-selection", "clipboard", "-t", mime, "-i", data=data, timeout=30)


def client_read(cell: Any, mime: str) -> bytes:
    return cell.xclient("xclip", "-selection", "clipboard", "-t", mime, "-o", timeout=30).stdout


def wait(fn: Any, ok: Any, timeout: float) -> Any:
    deadline, got = time.time() + timeout, fn()
    while time.time() < deadline and not ok(got):
        time.sleep(1.0)
        got = fn()
    return got


def run(cell: Any) -> None:
    R = cell.res
    page = cell.open()
    real = cell.engine in CLASS
    focus = Focus(cell)
    x11 = cell.backend == "x11" and "xclip" in cell.facts.get("tools", [])
    session = XclipSession(cell) if x11 else PageSession(cell)
    tag = f"{cell.transport}-{cell.backend}-{cell.engine}-{int(time.time())}"
    text = big_text(tag)
    image = noise_png(1100, 800, len(tag))
    if not session.prepare(page, text, image):
        R.check("the session's copying application is up", False, "neither xclip nor the session's Chrome")
        return

    # Session -> client: text, then an image, copied by an application of the session's.
    if real:
        R.check("the page has real focus after a click", focus.enter(page), "document.hasFocus() false")
    session.copy(page, "text")
    if real:
        got = wait(lambda: client_read(cell, "UTF8_STRING"), lambda b: b == text, 25)
        if got != text:
            page.mouse.click(900, 600)
            got = wait(lambda: client_read(cell, "UTF8_STRING"), lambda b: b == text, 10)
        R.check(f"{len(text) // 1024} KiB of text copied in the session lands in the local clipboard", got == text,
                f"{len(got)} bytes locally")
    else:
        page.mouse.click(900, 600)
        n = wait(lambda: page.evaluate("Math.max(0, ...window.__itWrites.filter(w => w.type.startsWith('text'))"
                                       ".map(w => w.size))"), lambda n: n >= len(text), 25)
        R.check(f"{len(text) // 1024} KiB of text copied in the session is written to the local clipboard",
                n >= len(text), f"largest text write {n}")
    session.copy(page, "image")
    if real:
        want = dims(image)
        got = wait(lambda: client_read(cell, "image/png"), lambda b: dims(b) == want, 25)
        if dims(got) != want:
            page.mouse.click(900, 600)
            got = wait(lambda: client_read(cell, "image/png"), lambda b: dims(b) == want, 10)
        R.check(f"a {len(image) // 1024} KiB image copied in the session lands in the local clipboard",
                dims(got) == want, f"{len(got)} bytes, {dims(got)}")

    # Client -> session: a local application's copy, pasted where the user pastes.
    if real:
        text2 = big_text(tag + "-up")
        client_own(cell, text2, "UTF8_STRING")
        focus.leave()
        focus.enter(page)
        if cell.engine != "chromium" and x11:
            page.keyboard.press("Control+v")
        want = (len(text2), hashlib.sha256(text2).hexdigest())
        got = wait(lambda: session.read(page, "text"), lambda g: g == want, 25 if x11 else 1)
        R.check(f"{len(text2) // 1024} KiB of text copied locally reaches the session's clipboard", got == want,
                f"{got[0]} bytes in the session")
        image2 = noise_png(1000, 700, len(tag) + 1)
        client_own(cell, image2, "image/png")
        focus.leave()
        focus.enter(page)
        if cell.engine != "chromium" and x11:
            page.keyboard.press("Control+v")
        got = wait(lambda: session.read(page, "image"), lambda g: g[1] == dims(image2), 30 if x11 else 1)
        R.check(f"a {len(image2) // 1024} KiB image copied locally reaches the session's clipboard",
                got[1] == dims(image2), f"{got[0]} bytes, {got[1]}")
    else:
        R.skip("local clipboard both ways", "headless WebKit has no X clipboard to copy from or into")

    # Upload Image: the panel's button and its file chooser.
    image3 = noise_png(900, 640, len(tag) + 2)
    open_sidebar(page)
    picked = TD.pick_clipboard_image(page, "classic", {"name": "upload.png", "mimeType": "image/png",
                                                       "buffer": image3})
    page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
    time.sleep(1.0)
    got = wait(lambda: session.read(page, "image"), lambda g: g[1] == dims(image3), 30 if x11 else 1)
    R.check("Upload Image puts the picked image on the session's clipboard", picked and got[1] == dims(image3),
            f"picked={picked}, {got[0]} bytes, {got[1]} in the session")
    session.done()
