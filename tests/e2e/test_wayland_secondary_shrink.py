#!/usr/bin/env python3
"""A left-hand secondary that shrinks keeps the second screen (Wayland).

Each display is a screen of the session compositor's own, and the compositor
refuses to move an output into room a live one still holds. A secondary placed
left of the primary sits at the origin, so when its rectangle shrinks -- a page
whose density changed, a smaller browser window -- the primary has to move into
the room that output gives up: the layout pass must shrink the output first and
grow it back only on the capture start that follows a move the other way, or the
second screen is dropped on every shrink. Driven against the in-process pixelflux
compositor on both transports: raw websockets clients on the websockets transport,
and two Chromium pages whose secondary viewport shrinks on the WebRTC transport,
where the page's own resize request carries the new rectangle.

Usage: python3 tests/e2e/test_wayland_secondary_shrink.py [websockets|webrtc]
"""
import asyncio
import importlib.util
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets


def settings_for(display_id: str, width: int, height: int, position: str = "left") -> dict:
    return {
        "displayId": display_id, "initialClientWidth": width, "initialClientHeight": height,
        "manual_resolution": False, "framerate": 30, "encoder": "jpeg",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": position,
    }


def loglen() -> int:
    return len(H.server_log())


async def wait_log_from(mark: int, substr: str, timeout: float = 20) -> bool:
    """Whether `substr` shows up in the server log at or after byte `mark`.

    Awaited rather than slept through: the sockets opened here are read by
    tasks on this loop, and blocking it stops them answering the keepalive
    their peer expects, which drops a connection mid-run.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        if H.server_log().find(substr, mark) >= 0:
            return True
        await asyncio.sleep(0.4)
    return False


def layouts_from(mark: int) -> list:
    """Every layout the server calculated at or after byte `mark`."""
    import ast
    out = []
    for line in H.server_log()[mark:].splitlines():
        if "Layout calculated" in line and "Layouts: " in line:
            try:
                out.append(ast.literal_eval(line.split("Layouts: ", 1)[1]))
            except (ValueError, SyntaxError):
                pass
    return out


async def drain(ws, seconds: float) -> list:
    """Text messages a socket receives within `seconds`; a socket the server closed
    yields what it got so far and a final KILL marker."""
    got = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            msg = await asyncio.wait_for(ws.recv(), timeout=0.5)
        except asyncio.TimeoutError:
            continue
        except websockets.ConnectionClosed as e:
            got.append(f"KILL socket closed: {e}")
            break
        if isinstance(msg, str):
            got.append(msg)
    return got


def wait_log_from_sync(mark: int, substr: str, timeout: float = 20) -> bool:
    """`wait_log_from` for the browser-driven variant, which runs no loop of its own."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if H.server_log().find(substr, mark) >= 0:
            return True
        time.sleep(0.4)
    return False


async def resize_secondary(res: "H.Results", primary, secondary, width: int, height: int,
                           primary_x: int, label: str) -> None:
    """Ask the secondary for a new size and check the pass that realizes it."""
    mark = loglen()
    try:
        await secondary.send(f"r,{width}x{height},display2")
    except websockets.ConnectionClosed as e:
        res.check(f"{label}: the secondary's socket is still open for its resize", False, str(e))
        return
    res.check(f"{label}: the secondary's capture follows the new size",
              await wait_log_from(mark, "Capture 'display2' followed the new layout live", 45)
              or await wait_log_from(mark, "SUCCESS: Capture started for 'display2'", 5), "")
    await drain(primary, 1.0)
    secondary_msgs = await drain(secondary, 2.0)
    tail = H.server_log()[mark:]
    res.check(f"{label}: the compositor refused no output move or creation",
              "RepositionOutput 0: rejected" not in tail and "CreateOutput 2: rejected" not in tail
              and "cannot create an output" not in tail and "cannot move the primary" not in tail,
              [line for line in tail.splitlines() if "rejected" in line][:3])
    res.check(f"{label}: the secondary was not dropped",
              "dropped on Wayland" not in tail
              and not any(m.startswith("KILL") for m in secondary_msgs), secondary_msgs[:3])
    res.check(f"{label}: the secondary output was kept rather than recreated",
              "Output 2 created" not in tail and "recreating it" not in tail,
              [line for line in tail.splitlines() if "Output 2" in line][:4])
    outputs = [line for line in tail.splitlines() if "[Wayland] Output" in line or "Configuring Output" in line]
    res.check(f"{label}: the compositor's outputs hold the new rectangles",
              any(f"{width}x{height}" in line and "Output 2" in line for line in outputs)
              and any(f"repositioned to ({primary_x}, 0)" in line for line in outputs), outputs[:6])
    layouts = layouts_from(mark)
    after = layouts[-1] if layouts else {}
    res.check(f"{label}: the layout carries the new secondary size and the moved primary",
              after.get("display2", {}).get("w") == width
              and after.get("primary", {}).get("x") == primary_x, after)


async def drive(res: "H.Results") -> None:
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as primary:
        await asyncio.wait_for(primary.recv(), timeout=10)
        mark = loglen()
        await primary.send("SETTINGS," + json.dumps(settings_for("primary", 1920, 1080)))
        res.check("primary capture starts",
                  await wait_log_from(mark, "SUCCESS: Capture started for 'primary'", 45), "")
        await drain(primary, 1.0)

        async with websockets.connect(uri, max_size=None) as secondary:
            await asyncio.wait_for(secondary.recv(), timeout=10)
            mark = loglen()
            await secondary.send("SETTINGS," + json.dumps(settings_for("display2", 1280, 720)))
            res.check("secondary capture starts left of the primary",
                      await wait_log_from(mark, "SUCCESS: Capture started for 'display2'", 45), "")
            layouts = layouts_from(mark)
            before = layouts[-1] if layouts else {}
            res.check("the primary is laid out at the secondary's right edge",
                      before.get("primary", {}).get("x") == 1280
                      and before.get("display2", {}).get("x") == 0, before)
            await drain(primary, 1.0)
            await drain(secondary, 1.0)

            await resize_secondary(res, primary, secondary, 960, 540, 960, "shrink")
            await resize_secondary(res, primary, secondary, 1280, 720, 1280, "grow back")
            await resize_secondary(res, primary, secondary, 1024, 800, 1024, "narrower and taller")

            await primary.send("STOP_VIDEO")
            await secondary.send("STOP_VIDEO")
            await asyncio.sleep(0.5)


def compositor_lines(mark: int) -> list:
    return [line for line in H.server_log()[mark:].splitlines()
            if "[Wayland] Output" in line or "Configuring Output" in line or "rejected" in line]


def drive_webrtc(res: "H.Results") -> None:
    """Two Chromium pages: the secondary, left of the primary, shrinks its viewport and
    the page asks the server for the smaller stream itself."""
    import core_lib as C
    from playwright.sync_api import sync_playwright
    with sync_playwright() as p:
        browser, page, _errors, _not_found = C.launch_chrome(p, mode="webrtc")
        try:
            res.check("primary video flows", bool(C.wait_wr_video(page, timeout=45)), "")
            mark = loglen()
            dpage = C.new_page(browser.contexts[0], mode="webrtc", url_hash="#display2-left")
            video = C.wait_wr_video(dpage, timeout=60)
            res.check("secondary video flows left of the primary", bool(video), video)
            res.check("the primary moved right of the secondary",
                      wait_log_from_sync(mark, "Output 0 repositioned to (1280, 0)", 20),
                      compositor_lines(mark)[:6])
            for label, (w, h), primary_x in (("shrink", (960, 540), 960),
                                             ("grow back", (1280, 720), 1280),
                                             ("narrower and taller", (1024, 800), 1024)):
                mark = loglen()
                dpage.set_viewport_size({"width": w, "height": h})
                moved = wait_log_from_sync(mark, f"Output 0 repositioned to ({primary_x}, 0)", 30)
                lines = compositor_lines(mark)
                res.check(f"{label}: the primary moved to the secondary's new edge", moved, lines[:6])
                res.check(f"{label}: the compositor refused no output move or creation",
                          not any("rejected" in line for line in lines), lines[:6])
                res.check(f"{label}: the secondary output was kept rather than recreated",
                          not any("Output 2 created" in line or "destroyed" in line for line in lines), lines[:6])
                tail = H.server_log()[mark:]
                res.check(f"{label}: the secondary was not dropped", "dropped on Wayland" not in tail, "")
                sized = False
                deadline = time.time() + 30
                while time.time() < deadline and not sized:
                    info = C.wait_wr_video(dpage, timeout=5) or {}
                    sized = info.get("w") == w
                res.check(f"{label}: the secondary's stream comes at the new size", sized, info)
            dpage.close()
        finally:
            C.close_browser(browser)


def main() -> "H.Results":
    which = sys.argv[1] if len(sys.argv) > 1 else "websockets"
    res = H.Results(f"wl-secondary-shrink-{which}")
    if importlib.util.find_spec("pixelflux") is None:
        H.skip_suite("pixelflux is not installed")
    H.server_start(mode=which, wayland=True)
    try:
        if which == "webrtc":
            drive_webrtc(res)
        else:
            asyncio.run(drive(res))
    finally:
        H.server_stop()
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
