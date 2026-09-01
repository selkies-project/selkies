#!/usr/bin/env python3
"""A single-display Wayland resize carries the nested session's screen with it.

Two compositors hold a size on this path: pixelflux's capture compositor, whose
screen the capture is a view over, and the nested session the applications
actually run on. The in-place resize sizes the first and restarts the capture;
only a DPI change ever sized the second, with whatever geometry was current
then. A client that changes device pixel ratio and back therefore ends with the
capture restored and the session — XWayland with it — still laid out for the
size the DPI transition left behind.

Driven against `on_resize_handler` with the capture and compositor calls
stubbed, so what is asserted is the order and the arguments, not the backend.
"""
import asyncio
import os
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
os.environ["SELKIES_FILE_MANAGER_PATH"] = tempfile.mkdtemp(prefix="selkies-wl-resize-")

import helpers as H  # noqa: E402

import selkies.selkies as S  # noqa: E402


class FakeInput:
    """Records what the session screen was asked to become."""

    def __init__(self) -> None:
        self.sized: list = []

    async def realize_wayland_dpi(self, dpi, display_id=None, size=None):
        self.sized.append((int(dpi), display_id, size))
        return 1.0


class FakeApp:
    """The shared app whose primary geometry mirrors display state."""

    def __init__(self) -> None:
        self.server_enable_resize = True
        self.display_width = 0
        self.display_height = 0


def make_server(realized):
    """A DataStreamingServer carrying only what the in-place resize path reads.

    Args:
        realized: `(width, height)` the compositor reports after the restart,
            which the session screen must end up at; None echoes the request,
            for driving a sequence of them.
    """
    srv = S.DataStreamingServer.__new__(S.DataStreamingServer)
    srv.cli_args = type("Args", (), {"manual_resolution": (False, False)})()
    srv.display_clients = {"primary": {"width": 1280, "height": 720,
                                       "scaling_dpi": 192, "scale": 1.0}}
    srv.display_layouts = {"primary": {"x": 0, "y": 0, "w": 1280, "h": 720}}
    srv.capture_instances = {}
    srv._video_capture_lock = asyncio.Lock()
    srv.input_handler = FakeInput()
    srv.capture_screen = []

    async def size_wayland_screen(width, height, grow_only=False):
        srv.capture_screen.append((width, height, grow_only))

    async def sync_realized(display_id, broadcast=True):
        if realized is not None:
            srv.display_clients[display_id]["width"] = realized[0]
            srv.display_clients[display_id]["height"] = realized[1]

    async def stop_capture(display_id):
        return None

    async def start_capture(display_id, width, height, x_offset, y_offset):
        return None

    srv._size_wayland_screen = size_wayland_screen
    srv._sync_wayland_realized_geometry = sync_realized
    srv._stop_capture_for_display = stop_capture
    srv._start_capture_for_display = start_capture
    return srv


async def scenario(res: H.Results) -> None:
    was_wayland = S.IS_WAYLAND
    S.IS_WAYLAND = True
    try:
        # The compositor even-masks to 1920x928; the session must land there too,
        # not on the 1920x936 that was asked for.
        srv = make_server((1920, 928))
        await S.on_resize_handler("1920x936", FakeApp(), srv, "primary")

        res.check("the capture screen is grown, then fitted to what was realized",
                  srv.capture_screen == [(1920, 936, True), (1920, 928, False)],
                  srv.capture_screen)
        res.check("the nested session's screen is sized once",
                  len(srv.input_handler.sized) == 1, srv.input_handler.sized)
        res.check("it is sized to the realized geometry, not the request",
                  srv.input_handler.sized[-1][2] == (1920, 928),
                  srv.input_handler.sized[-1])
        res.check("at the DPI the display is holding",
                  srv.input_handler.sized[-1][0] == 192, srv.input_handler.sized[-1])
        res.check("for the display being resized",
                  srv.input_handler.sized[-1][1] == "primary", srv.input_handler.sized[-1])

        # The reported sequence: at DPR 2 the media resizes, at DPR 1 it comes
        # back, and the session screen has to arrive at both.
        srv = make_server(None)
        srv.display_clients["primary"].update({"width": 1920, "height": 936})
        await S.on_resize_handler("2400x1440", FakeApp(), srv, "primary")
        grew = srv.input_handler.sized[-1][2]
        srv.display_clients["primary"]["scaling_dpi"] = 96
        await S.on_resize_handler("1920x936", FakeApp(), srv, "primary")
        res.check("a resize after a DPI change moves the session screen with it",
                  grew == (2400, 1440) and srv.input_handler.sized[-1][2] == (1920, 936),
                  srv.input_handler.sized)

        # A redundant request returns before touching either compositor.
        srv = make_server((1920, 928))
        srv.display_clients["primary"].update({"width": 1920, "height": 936})
        await S.on_resize_handler("1920x936", FakeApp(), srv, "primary")
        res.check("a redundant resize sizes nothing",
                  not srv.capture_screen and not srv.input_handler.sized,
                  f"{srv.capture_screen} {srv.input_handler.sized}")

        # Without an input handler the path still completes.
        srv = make_server((1920, 928))
        srv.input_handler = None
        await S.on_resize_handler("1920x936", FakeApp(), srv, "primary")
        res.check("no input handler leaves the capture path intact",
                  srv.capture_screen == [(1920, 936, True), (1920, 928, False)],
                  srv.capture_screen)
    finally:
        S.IS_WAYLAND = was_wayland


def run() -> H.Results:
    res = H.Results("wayland-resize-session-screen")
    asyncio.run(scenario(res))
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
