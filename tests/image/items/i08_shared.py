"""8. Test `#shared` and `#player*`. For the shared window, only the screen/audio should work and nothing else. Check no long-term screen quality degradations. For controllers, the gamepad should also work.

A `#shared` page beside the controller's must show the screen and play the
session's sound, and must not move the session's pointer, type into it, put
its clipboard into the session, offer uploads, or drive a gamepad. Its
picture of a still, detailed screen must not get worse over the run (the
error against the known picture, sampled over `E2E_IMAGE_DRIFT_SECONDS`,
default 120; set it to tens of minutes for the long check). `#player2` to
`#player4` must each drive a pad the session's tester sees.
"""
import os
import time
from typing import Any

from image_lib import H, audio_level, pattern_matches, session_browser, stop_session_browser

ITEM = 8
TITLE = "#shared viewer and #player controllers"
DRIFT = float(os.environ.get("E2E_IMAGE_DRIFT_SECONDS", "120"))


def run(cell: Any) -> None:
    R = cell.res
    owner = cell.open()
    shared = cell.open("#shared", init=(H.pad_init_js(),))
    s = cell.wait_pattern(shared, 30)
    R.check("#shared shows the session's screen", pattern_matches(s), s and s["points"])
    cell.go("/tone.html")
    level = audio_level(shared, 10)
    R.check("#shared plays the session's sound", level > 0, f"level {level}")
    cell.go("/pattern.html")
    R.check("#shared renders no dashboard and no upload control",
            shared.locator('button:has-text("Upload Files")').count() == 0
            and shared.locator(".sidebar").count() == 0, "")

    # Input from the viewer must not reach the session; the owner's must (the control).
    # The session's pattern page counts the pointer moves its application gets.
    # Each page is clicked first, as a user does on switching to it: a page without
    # focus may send nothing, which would pass the viewer's check for no reason.
    shared.bring_to_front()
    shared.mouse.click(400, 400)
    time.sleep(1.0)
    before = (cell.report("pointer", 5) or {}).get("moves", 0)
    for x, y in ((200, 200), (900, 500), (300, 600)):
        shared.mouse.move(x, y)
        time.sleep(0.3)
    shared.mouse.click(400, 400)
    time.sleep(1.5)
    after = (cell.report("pointer", 5) or {}).get("moves", 0)
    owner.bring_to_front()
    owner.mouse.click(611, 333)
    owner.mouse.move(640, 360)
    owner.mouse.move(700, 400)
    time.sleep(1.5)
    moved = (cell.report("pointer", 5) or {}).get("moves", 0)
    R.check("the viewer's pointer does not move the session's, the owner's does",
            after == before and moved > after, f"moves {before} -> {after} (viewer) -> {moved} (owner)")
    cell.go("/ime.html")
    time.sleep(2)
    shared.bring_to_front()
    shared.mouse.click(640, 360)
    shared.keyboard.type("viewer")
    time.sleep(1.5)
    typed = (cell.report("ime", 5) or {}).get("value", "")
    R.check("the viewer's keys do not reach the session", "viewer" not in typed, repr(typed[-40:]))
    cell.go("/pattern.html")

    # The viewer's clipboard stays out of the session.
    if cell.engine in ("chromium", "firefox") and "xclip" in cell.facts.get("tools", []):
        cell.target.sh("printf session-own | (setsid xclip -selection clipboard -i > /dev/null 2>&1 &)")
        time.sleep(1)
        cell.xclient("xclip", "-selection", "clipboard", "-i", data=b"viewer-clip")
        shared.bring_to_front()
        shared.mouse.click(640, 360)
        shared.keyboard.press("Control+v")
        time.sleep(3)
        clip = cell.target.out("timeout 5 xclip -selection clipboard -o 2>/dev/null")
        R.check("the viewer's clipboard does not reach the session", "viewer-clip" not in clip, repr(clip[:40]))
    else:
        R.skip("the viewer's clipboard", "needs a real client clipboard and xclip in the session")

    # Gamepads: the viewer's pad is refused, a controller's reaches the session.
    session_browser(cell.target, "chrome", "/gamepad.html", kiosk=False, profile="pad")
    time.sleep(4)
    cell.clear_report("gamepad")
    for i in (0, 1, 2):
        shared.evaluate(f"window.__padPress({i}, 1)")
        time.sleep(0.2)
        shared.evaluate(f"window.__padPress({i}, 0)")
    time.sleep(2)
    seen = cell.report("gamepad", 5) or {}
    R.check("the viewer's gamepad does not reach the session",
            not any(p.get("pressed") for p in seen.values()), seen)
    shared.close()
    for n in (2, 3, 4):
        cell.go("/gamepad.html", ch="pad")
        time.sleep(3)
        cell.clear_report("gamepad")
        player = cell.open(f"#player{n}", init=(H.pad_init_js(),))
        time.sleep(2)
        for i in (0, 3, 12):
            player.evaluate(f"window.__padPress({i}, 1)")
            time.sleep(0.2)
            player.evaluate(f"window.__padPress({i}, 0)")
            time.sleep(0.15)
        time.sleep(2)
        seen = cell.report("gamepad", 5) or {}
        pressed = set().union(*[set(p.get("pressed", [])) for p in seen.values()]) if seen else set()
        R.check(f"#player{n}'s gamepad reaches the session", {0, 3, 12} <= pressed,
                {k: (p.get("id", "")[:30], p.get("pressed")) for k, p in seen.items()})
        player.close()
    stop_session_browser(cell.target, "chrome", "pad")

    # Long-term quality on a still, detailed screen, as the viewer sees it.
    cell.go("/texture.html")
    shared = cell.open("#shared")
    time.sleep(8)
    first = cell.texture_error(shared)
    errors = [first]
    deadline = time.time() + DRIFT
    while time.time() < deadline:
        time.sleep(min(15, max(1, deadline - time.time())))
        errors.append(cell.texture_error(shared))
    valid = [e for e in errors if e is not None]
    # A sample caught mid-refresh is not drift: the later samples' median and the last one decide.
    later = sorted(valid[1:])
    R.check(f"the viewer's picture holds its quality over {DRIFT:.0f} s", len(valid) >= 2 and
            later[len(later) // 2] <= valid[0] + 1.0 and valid[-1] <= valid[0] + 1.0, f"error {valid}")
    shared.close()
    cell.go("/pattern.html")
