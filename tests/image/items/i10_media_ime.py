"""10. Test audio/videos or anything else on the web browsers. Moreover, you should test IME in your own language.

A clip with a moving picture and a tone plays in the session's own Chrome and
Firefox: it has to play there, its sound has to reach the client, and its
picture has to keep changing in the stream. Then Korean is composed through
an IME on the client (Chrome's IME composition over CDP) and has to land in
a text field on the session's desktop. Firefox's and WebKit's drivers offer
no composition, so there it is not checked.
"""
import time
from typing import Any

from image_lib import audio_level, session_browser, stop_session_browser

ITEM = 10
TITLE = "media playback in the session's browsers, IME"
WORD = "한국어 입력"


def changing(cell: Any, page: Any, secs: float = 4) -> int:
    """How many of several samples of the stream's picture differ from the one before."""
    last, changes = None, 0
    for _ in range(int(secs / 0.5)):
        s = cell.sample(page, [(x / 5, y / 4) for x in range(1, 5) for y in range(1, 4)])
        sig = s and s["points"]
        if last is not None and sig != last:
            changes += 1
        last = sig
        time.sleep(0.5)
    return changes


def run(cell: Any) -> None:
    R = cell.res
    page = cell.open()
    page.mouse.click(900, 600)
    for which in ("chrome", "firefox"):
        cell.clear_report("play")
        if not session_browser(cell.target, which, "/play.html", kiosk=True, profile="play"):
            R.skip(f"playback in the session's {which}", "not installed in the image")
            continue
        rep = cell.report("play", 25, where=lambda r: r.get("t", 0) > 1.0 and not r.get("paused"))
        R.check(f"a clip plays in the session's {which}", rep is not None, cell.report("play", 1))
        level = audio_level(page, 8)
        R.check(f"its sound reaches the client from the session's {which}", level > 0, f"level {level}")
        moved = changing(cell, page)
        R.check(f"its picture keeps moving in the stream from the session's {which}", moved >= 3, f"{moved} of 7 changed")
        stop_session_browser(cell.target, which, "play")
    # IME: composed on the client, committed into a field on the session's desktop.
    cell.go("/ime.html")
    time.sleep(3)
    cell.clear_report("ime")
    page.mouse.click(640, 360)
    time.sleep(0.5)
    if cell.engine == "chromium":
        cdp = cell.ctx.new_cdp_session(page)
        for i in range(1, len(WORD) + 1):
            cdp.send("Input.imeSetComposition", {"text": WORD[:i], "selectionStart": i, "selectionEnd": i})
            time.sleep(0.05)
        cdp.send("Input.insertText", {"text": WORD})
    else:
        R.skip("Korean composed on the client", f"{cell.engine}'s driver offers no IME composition, and the "
               "client takes no scripted composition events for a real one")
        cell.go("/pattern.html")
        return
    rep = cell.report("ime", 15, where=lambda r: WORD in r.get("value", ""))
    R.check("Korean composed on the client lands in a field on the session's desktop", rep is not None,
            repr((cell.report("ime", 1) or {}).get("value", "")[-30:]))
    cell.go("/pattern.html")
