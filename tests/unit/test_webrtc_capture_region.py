#!/usr/bin/env python3
"""WebRTC's X11 captures across a root that changes size under them.

A capture pinned to a region (a realized mode wider than the page asked for,
a display of an extended layout) moves to its new rectangle before the root
shrinks under it, the root growing first where that rectangle lies past the
current one, as the websockets reconfigure does: a grab past the root's edge
fails, and NvFBC cannot rebuild a session whose box the root no longer holds
(pixelflux#56). A capture that follows the root is left to follow it. A
capture that ended on its own starts again on the next resize or consumer
start. Driven with fakes: no X server, pixelflux, or browser.
"""
import asyncio
import os
import sys
from types import SimpleNamespace

for key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[key]

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

from selkies import webrtc_mode  # noqa: E402
from selkies.webrtc_media_pipeline import MediaPipelinePixel  # noqa: E402


class Pipeline:
    """A MediaPipelinePixel stand-in logging region moves into the shared `log`."""

    def __init__(self, log: list, did: str, w: int, h: int, region=(0, 0), capturing: bool = True) -> None:
        self.log, self.did = log, did
        self.width, self.height, self.capture_region = w, h, region
        self.capturing = capturing
        self.framerate = 60
        self.last_resize_success = True
        self.capture_module = SimpleNamespace(update_capture_region=lambda *a: log.append(("follow", did) + a))

    def is_screen_capturing(self) -> bool:
        return self.capturing

    async def update_capture_region(self, x: int, y: int, w: int, h: int) -> None:
        self.log.append(("region", self.did, x, y, w, h))
        self.capture_region, self.width, self.height = (x, y), w, h

    async def revive_screen_capture(self) -> bool:
        self.log.append(("revive", self.did))
        return False


def service(log: list, root: tuple, rounding: int = 8):
    """A WebRTC service whose X server starts at `root` and realizes modes in `rounding`-pixel cells."""
    svc = webrtc_mode.WebRTCService(SimpleNamespace(set_clients_present=lambda present: None))
    svc.settings = SimpleNamespace(manual_resolution=(False, False), second_screen=(True, False))
    svc.args = SimpleNamespace(force_aligned_resolution=False)
    svc.rtc_app = SimpleNamespace(send_remote_resolution=lambda res, did: log.append(("told", did, res)))
    state = {"root": root}

    async def get_new_res(_res):
        return "{}x{}".format(*state["root"]), _res, [], "7680x4320", "DVI-D-0"

    async def resize_display(res, _refresh=None, _output=None):
        w, h = (int(v) for v in res.split("x"))
        w = (w + rounding - 1) // rounding * rounding
        log.append(("root", w, h))
        state["root"] = (w, h)
        return w, h

    async def grow_framebuffer(w, h):
        log.append(("grow", w, h))
        state["root"] = (w, h)
        return True

    webrtc_mode.get_new_res, webrtc_mode.resize_display = get_new_res, resize_display
    webrtc_mode.grow_framebuffer = grow_framebuffer
    return svc, state


def region_past_root(log: list, start_root: tuple, pipelines: dict) -> list:
    """Replay `log` and list every moment a live pinned capture lay past the root."""
    root = start_root
    rects = {did: (p0[0], p0[1], p0[2], p0[3]) for did, p0 in pipelines.items()}
    bad = []
    for step in log:
        if step[0] in ("root", "grow"):
            root = step[1:3]
        elif step[0] == "region":
            rects[step[1]] = step[2:6]
        for did, (x, y, w, h) in rects.items():
            if x + w > root[0] or y + h > root[1]:
                bad.append((step, did, (x, y, w, h), root))
    return bad


async def primary_block(res: H.Results) -> None:
    webrtc_mode.IS_WAYLAND = False

    # The reporter's case: a 2366-wide request realized 2368 wide pins the region; the next shrink moves it first.
    log: list = []
    svc, state = service(log, (2368, 1304))
    svc.media_pipeline = Pipeline(log, "primary", 2366, 1300)
    svc.display_pipelines = {"primary": svc.media_pipeline}
    await svc._resize_primary_display("1918x1000")
    steps = [s[0] for s in log]
    res.check("shrink: the pinned region moves before the root shrinks",
              steps[:2] == ["region", "root"] and log[0] == ("region", "primary", 0, 0, 1918, 1000), log)
    res.check("shrink: no moment has the region past the root",
              not region_past_root(log, (2368, 1304), {"primary": (0, 0, 2366, 1300)}), log)
    res.check("shrink: the region ends on the request inside the rounded root, the page told",
              svc.media_pipeline.capture_region == (0, 0) and (svc.media_pipeline.width,
                                                              svc.media_pipeline.height) == (1918, 1000)
              and ("told", "primary", "1918x1000") in log and state["root"] == (1920, 1000), log)
    res.check("shrink: a capture that ended is looked at after the resize", ("revive", "primary") in log, log)

    # A grow keeps main's order: the root first, the region after.
    log.clear()
    svc, state = service(log, (1920, 1000))
    svc.media_pipeline = Pipeline(log, "primary", 1918, 1000)
    svc.display_pipelines = {"primary": svc.media_pipeline}
    await svc._resize_primary_display("2366x1300")
    res.check("grow: the root grows before the region follows",
              [s[0] for s in log][:2] == ["root", "region"]
              and not region_past_root(log, (1920, 1000), {"primary": (0, 0, 1918, 1000)}), log)

    # Wider but shorter: the root grows to hold both rectangles, the region moves, then the mode is set.
    log.clear()
    svc, state = service(log, (1920, 1080))
    svc.media_pipeline = Pipeline(log, "primary", 1918, 1080)
    svc.display_pipelines = {"primary": svc.media_pipeline}
    await svc._resize_primary_display("2366x900")
    res.check("wider and shorter: grow to the union, move, then set the mode",
              [s[0] for s in log][:3] == ["grow", "region", "root"] and log[0] == ("grow", 2366, 1080)
              and not region_past_root(log, (1920, 1080), {"primary": (0, 0, 1918, 1080)}), log)

    # A capture following the root (no region) is left to it, as on main.
    log.clear()
    svc, state = service(log, (1920, 1080))
    svc.media_pipeline = Pipeline(log, "primary", 1920, 1080, region=None)
    svc.display_pipelines = {"primary": svc.media_pipeline}
    await svc._resize_primary_display("1280x720")
    res.check("root-follow: no region move ahead of the shrink",
              not any(s[0] in ("region", "grow") for s in log) and log[0] == ("root", 1280, 720), log)

    # Nothing captured, nothing moved.
    log.clear()
    svc, state = service(log, (2368, 1304))
    svc.media_pipeline = Pipeline(log, "primary", 2366, 1300, capturing=False)
    svc.display_pipelines = {"primary": svc.media_pipeline}
    await svc._resize_primary_display("1918x1000")
    res.check("no capture: the root alone changes", log[0] == ("root", 1920, 1000), log)


async def layout_block(res: H.Results) -> None:
    webrtc_mode.IS_WAYLAND = False
    # A secondary on the left narrows from 1280 to 800: the primary at x=1280 moves to x=800 before the root
    # shrinks from 3200 to 2720; the secondary's own rectangle stays inside and moves after, as on main.
    log: list = []
    svc, state = service(log, (3200, 1080))
    primary = Pipeline(log, "primary", 1920, 1080, region=(1280, 0))
    secondary = Pipeline(log, "display2", 1280, 1080, region=(0, 0))
    svc.media_pipeline = primary
    svc.display_pipelines = {"primary": primary, "display2": secondary}
    await svc._retarget_ahead_of_root({"primary": (800, 0, 1920, 1080), "display2": (0, 0, 800, 1080)}, 2720, 1080)
    res.check("layout: the display the narrower root leaves out moves first, no grow needed",
              log == [("region", "primary", 800, 0, 1920, 1080)], log)

    # Both shrink and grow at once: the root grows to hold the new rectangle before the move.
    log.clear()
    svc, state = service(log, (3200, 1080))
    primary = Pipeline(log, "primary", 1920, 1080, region=(1280, 0))
    svc.media_pipeline = primary
    svc.display_pipelines = {"primary": primary}
    await svc._retarget_ahead_of_root({"primary": (800, 0, 1920, 1200)}, 2720, 1200)
    res.check("layout: a taller rectangle grows the root before the move",
              log == [("grow", 3200, 1200), ("region", "primary", 800, 0, 1920, 1200)], log)

    # A refused grow clamps the move to the root there is.
    log.clear()
    svc, state = service(log, (3200, 1080))

    async def refused(w, h):
        log.append(("refused", w, h))
        return False

    webrtc_mode.grow_framebuffer = refused
    primary = Pipeline(log, "primary", 1920, 1080, region=(1280, 0))
    svc.media_pipeline = primary
    svc.display_pipelines = {"primary": primary}
    await svc._retarget_ahead_of_root({"primary": (800, 0, 1920, 1200)}, 2720, 1200)
    res.check("layout: a refused grow moves the region inside the current root",
              log == [("refused", 3200, 1200), ("region", "primary", 800, 0, 1920, 1080)], log)


class Module:
    def __init__(self, capturing: bool) -> None:
        self.is_capturing = capturing


async def revive_block(res: H.Results) -> None:
    def pipeline(capturing: bool, module_alive: bool):
        p = MediaPipelinePixel(async_event_loop=asyncio.get_running_loop(), encoder="h264enc", audio_enabled=False)
        p.log = []
        p._is_screen_capturing = capturing
        p.capture_module = Module(module_alive)

        async def start():
            p.log.append("start")
            p.capture_module = Module(True)
            p._is_screen_capturing = True

        async def stop():
            p.log.append("stop")
            p.capture_module = None
            p._is_screen_capturing = False

        p.start_screen_capture, p.stop_screen_capture = start, stop
        return p

    from selkies import webrtc_media_pipeline as wmp
    wayland = wmp.app_settings.wayland
    try:
        wmp.app_settings.wayland = (False, False)
        p = pipeline(True, False)
        res.check("revive: an X11 capture that ended starts again",
                  await p.revive_screen_capture() and p.log == ["stop", "start"] and not p.capture_ended(), p.log)
        p = pipeline(True, True)
        res.check("revive: a live capture is left alone", not await p.revive_screen_capture() and not p.log, p.log)
        p = pipeline(False, False)
        res.check("revive: a paused capture stays paused", not await p.revive_screen_capture() and not p.log, p.log)
        p = pipeline(True, False)
        p.capture_module = SimpleNamespace()
        res.check("revive: a module that cannot tell is assumed alive",
                  not await p.revive_screen_capture() and not p.log, p.log)
        wmp.app_settings.wayland = (True, False)
        p = pipeline(True, False)
        res.check("revive: Wayland is left to its host reap", not await p.revive_screen_capture() and not p.log,
                  p.log)
    finally:
        wmp.app_settings.wayland = wayland

    # A consumer joining a running pipeline whose capture ended brings it back.
    webrtc_mode.IS_WAYLAND = False
    svc = webrtc_mode.WebRTCService(SimpleNamespace(set_clients_present=lambda present: None))
    svc.settings = SimpleNamespace(second_screen=(False, False), wayland_host_display="",
                                   manual_resolution=(False, False))
    revived = []

    class Running(Pipeline):
        async def start_media_pipeline(self, video=True, audio=True):
            self.log.append(("start", video, audio))

        def is_media_pipeline_running(self):
            return True

        async def revive_screen_capture(self):
            revived.append(True)
            return True

    log: list = []
    svc.media_pipeline = Running(log, "primary", 1920, 1080)
    svc.display_pipelines = {"primary": svc.media_pipeline}
    svc.rtc_app = SimpleNamespace(peer_connections={}, send_media_data_over_channel=lambda *a: None)
    svc._primary_sized.set()

    async def none(*_a, **_k):
        return False

    svc._settle_primary_consumers = none
    svc._refresh_second_screen_capacity = none
    await asyncio.wait_for(svc.start_display_media("primary"), 2)
    res.check("revive: a consumer's start looks at the running capture", revived == [True], log)


async def main_async(res: H.Results) -> None:
    await primary_block(res)
    await layout_block(res)
    await revive_block(res)


def main() -> int:
    res = H.Results("webrtc-capture-region")
    asyncio.run(main_async(res))
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
