#!/usr/bin/env python3
"""The session's clipboard coming back from the browser is no copy.

What the session copies reaches the browser's clipboard, and the browser can
hand it back changed in form only: Chromium on Windows writes plain text with
CRLF line ends, and markup comes back in the engine's own wrapping. Read on
focus, or on the paste chord the page holds until its clipboard is sent, that
form must not go back over the session's clipboard, or a file manager's cut
(a URI list marked as a cut beside its plain text) becomes plain text, and the
file manager's paste offers to save the clipboard as a new file instead of
moving the files. A session application stands in for the file manager here,
owning the clipboard with the formats Dolphin offers for a cut.
Usage: python3 tests/e2e/test_clipboard_echo.py [websockets|webrtc|all]
"""
import os
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

sys.path.insert(0, os.path.join(H.REPO, "src"))
from selkies.Xlib import X, Xatom, display as xdisp  # noqa: E402
from selkies.Xlib.protocol import event as xevent  # noqa: E402
import test_clipboard_rich as R  # noqa: E402

DASH = os.path.join(H.REPO, "addons/selkies-dashboard/dist")
FILES = ["file:///home/ubuntu/Documents/report.odt", "file:///home/ubuntu/Documents/notes.txt"]
# Dolphin's cut: the URI list (CRLF-separated, as the format wants), the cut mark, and the paths as text.
CUT = [("text/uri-list", ("\r\n".join(FILES) + "\r\n").encode()),
       ("application/x-kde-cutselection", b"1"),
       ("text/plain", "\n".join(FILES).encode()),
       ("UTF8_STRING", "\n".join(FILES).encode())]
HTML, PLAIN = "<p>from <b>the session</b></p>", "from the session"


class Owner:
    """A session application owning the clipboard with any formats, as a file
    manager does for a cut: it answers TARGETS and each format, and knows when
    something else took the clipboard."""

    def __init__(self, display: str, formats) -> None:
        self.d = xdisp.Display(display)
        screen = self.d.screen()
        self.win = screen.root.create_window(0, 0, 1, 1, 0, screen.root_depth, window_class=X.InputOutput)
        self.clip = self.d.get_atom("CLIPBOARD")
        self.formats = {self.d.get_atom(m): data for m, data in formats}
        self.targets = self.d.get_atom("TARGETS")
        self.win.set_selection_owner(self.clip, X.CurrentTime)
        self.d.flush()
        self.lost = False
        self.stop = False
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self) -> None:
        while not self.stop:
            if not self.d.pending_events():
                time.sleep(0.005)
                continue
            e = self.d.next_event()
            if e.type == X.SelectionClear:
                self.lost = True
            elif e.type == X.SelectionRequest:
                prop = e.property if e.property != X.NONE else e.target
                if e.target == self.targets:
                    e.requestor.change_property(prop, Xatom.ATOM, 32, [self.targets] + list(self.formats))
                elif e.target in self.formats:
                    e.requestor.change_property(prop, e.target, 8, self.formats[e.target])
                else:
                    prop = X.NONE
                e.requestor.send_event(xevent.SelectionNotify(time=e.time, requestor=e.requestor, selection=e.selection,
                                                              target=e.target, property=prop))
                self.d.flush()

    def owns(self) -> bool:
        return not self.lost and self.d.get_selection_owner(self.clip) == self.win

    def close(self) -> None:
        self.stop = True
        self.thread.join(1)
        self.d.close()


def page_with_clipboard(p, mode: str):
    """A Chromium page on the session, allowed to read and write the clipboard."""
    browser = C.launch_browser(p, "chromium")
    ctx = browser.new_context(viewport={"width": 1440, "height": 900})
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.grant_permissions(["clipboard-read", "clipboard-write"], origin=H.BASE_URL)
    page = ctx.new_page()
    page.goto(H.BASE_URL, wait_until="load")
    time.sleep(12.0)
    return browser, page


def gestures(page, n: int = 2) -> None:
    """Clicks on the stream: the local write needs a user activation, and the
    payload must have arrived before the one that lands it."""
    for _ in range(n):
        page.mouse.move(700, 450)
        page.mouse.down()
        page.mouse.up()
        time.sleep(2.0)


def block(mode: str) -> "H.Results":
    res = H.Results(f"clipecho-{mode}")
    H.server_start(mode=mode, wayland=False, web_root=DASH,
                   extra_env={"PYTHONPATH": os.path.join(H.REPO, "src")})
    with sync_playwright() as p:
        browser, page = page_with_clipboard(p, mode)
        source = None
        try:
            source = Owner(H.require_display(), CUT)
            gestures(page)
            landed = page.evaluate("navigator.clipboard.readText().catch((e) => 'read failed: ' + e.name)")
            res.check("a cut in the session reaches the browser as its paths",
                      landed == "\n".join(FILES), landed)
            # What Chromium on Windows keeps for that text: the same, with CRLF line ends.
            page.evaluate("(t) => navigator.clipboard.writeText(t)", "\r\n".join(FILES))
            page.mouse.click(700, 450)
            page.keyboard.press("Control+v")
            time.sleep(2.5)
            page.evaluate("window.dispatchEvent(new Event('focus'))")
            time.sleep(2.5)
            res.check("the paste chord leaves the session's cut its own (CRLF read-back)",
                      source.owns(), "" if source.owns() else "the session's clipboard was taken over")
            mark = R.convert("application/x-kde-cutselection")
            res.check("the file manager still finds its cut mark", mark == b"1", mark)

            source.close()
            source = Owner(H.require_display(), [("text/html", HTML.encode()), ("text/plain", PLAIN.encode()),
                                                 ("UTF8_STRING", PLAIN.encode())])
            gestures(page)
            local = R.read_rich(page)
            res.check("a formatted copy in the session reaches the browser",
                      isinstance(local, dict) and local.get("text") == PLAIN, local)
            page.evaluate("window.dispatchEvent(new Event('focus'))")
            time.sleep(2.5)
            page.mouse.click(700, 450)
            page.keyboard.press("Control+v")
            time.sleep(2.5)
            res.check("its markup read back is no copy: the session keeps its own",
                      source.owns(), "" if source.owns() else "the session's clipboard was taken over")

            # Control: a copy made in the browser afterwards still reaches the session.
            page.evaluate("(t) => navigator.clipboard.writeText(t)", "copied in the browser")
            page.evaluate("window.dispatchEvent(new Event('focus'))")
            time.sleep(2.5)
            plain = R.convert("UTF8_STRING")
            res.check("a real copy in the browser still reaches the session",
                      plain == b"copied in the browser", plain)
        finally:
            source.close() if source is not None else None
            browser.close()
            H.server_stop()
    return res


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    modes = ["websockets", "webrtc"] if which == "all" else [which]
    ok = all([block(m).summary() for m in modes])
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
