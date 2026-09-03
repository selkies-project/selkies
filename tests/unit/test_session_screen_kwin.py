#!/usr/bin/env python3
"""A nested KWin session grows and retires screens as virtual outputs.

KWin serves no control socket, so the labwc rung of the session-screen
control never answers there; its screens exist on demand as
`zkde_screencast_unstable_v1` virtual outputs, reached through pixelflux. The
protocol is also served by a stock kwin that registers no nested virtual
output, so that rung is proven by growing a probe screen, once per session
compositor.

Drives the session-screen handler with pixelflux and the settings stubbed, so
what is asserted is which rung is consulted and with what, not a compositor.
"""
import asyncio
import logging
import os
import sys
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
# No control socket in an empty runtime dir: the labwc rung answers absent.
os.environ["XDG_RUNTIME_DIR"] = tempfile.mkdtemp(prefix="selkies-kwin-")
import helpers as H

from selkies.input_handler import WebRTCInput

CAPTURE = "wayland-1"
SESSION = "wayland-0"


class FakePixelflux:
    """The app-screen calls of pixelflux, recording what was asked."""

    def __init__(self, kde: bool = True) -> None:
        self.kde = kde
        self.calls: list = []
        self.screens: list = [("WL-1", 0, 0, 1920, 1080)]
        self.parked = 0
        self.fail_add: bool = False
        self.fail_control: bool = False

    def app_screen_control_available(self, display):
        self.calls.append(("control", display))
        if self.fail_control:
            raise RuntimeError("connect: no such socket")
        return self.kde

    def list_app_screens(self, display):
        self.calls.append(("list", display))
        return list(self.screens)

    def add_app_screen(self, display, name, width, height, scale):
        self.calls.append(("add", display, name, width, height, scale))
        if self.fail_add:
            raise RuntimeError("virtual screen refused")
        self.screens.append((name, 1920, 0, width, height))
        self.parked += 1

    def remove_app_screen(self, name):
        self.calls.append(("remove", name))
        held = [s for s in self.screens if s[0] == name]
        self.screens = [s for s in self.screens if s[0] != name]
        return bool(held)

    def list_windows(self):
        return [("w", 0, 0, 0, True)] * self.parked


class Log(logging.Handler):
    def __init__(self) -> None:
        super().__init__()
        self.lines: list = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append(record.getMessage())


def make_handler(pixelflux: FakePixelflux) -> WebRTCInput:
    handler = object.__new__(WebRTCInput)
    handler.is_wayland = True
    handler.wayland_input = pixelflux
    handler._wayland_display_name = lambda: CAPTURE
    handler._app_wayland_display = lambda: SESSION
    return handler


async def scenario(res: "H.Results") -> None:
    log = Log()
    logger = logging.getLogger("webrtc_input")
    logger.addHandler(log)
    logger.setLevel(logging.DEBUG)
    try:
        pf = FakePixelflux(kde=False)
        handler = make_handler(pf)
        ok, why = await handler.probe_session_screen_capability()
        res.check("a stock kwin holding one screen offers no second display",
                  not ok and "no screen control" in why, (ok, why))
        res.check("after its protocol was probed",
                  ("control", SESSION) in pf.calls, pf.calls)
        res.check("and the screen count decided",
                  any(c[0] == "list" for c in pf.calls), pf.calls)
        pf.calls.clear()
        await handler.probe_session_screen_capability()
        res.check("a re-probe keeps that answer rather than growing another probe screen",
                  not any(c[0] == "control" for c in pf.calls), pf.calls)

        pf = FakePixelflux()
        pf.fail_control = True
        handler = make_handler(pf)
        await handler.probe_session_screen_capability()
        pf.fail_control = False
        pf.calls.clear()
        ok, why = await handler.probe_session_screen_capability()
        res.check("an unreachable compositor is asked again",
                  ok and ("control", SESSION) in pf.calls, (ok, why, pf.calls))

        pf = FakePixelflux()
        handler = make_handler(pf)
        ok, why = await handler.probe_session_screen_capability()
        res.check("a kwin that registers the probe screen answers the virtual-output rung",
                  ok and pf.calls == [("control", SESSION)], (ok, why, pf.calls))
        res.check("and the control is what the handler reports",
                  handler.session_screen_control_available()
                  and not handler.session_screen_ipc_available(), None)
        pf.calls.clear()
        await handler.probe_session_screen_capability()
        res.check("the answer is kept while the session socket is the same",
                  not any(c[0] == "control" for c in pf.calls), pf.calls)
        socket_path = os.path.join(os.environ["XDG_RUNTIME_DIR"], SESSION)
        open(socket_path, "w").close()
        pf.calls.clear()
        await handler.probe_session_screen_capability()
        res.check("a restarted session compositor, a new socket, is probed afresh",
                  ("control", SESSION) in pf.calls, pf.calls)

        pf.calls.clear()
        await handler.ensure_session_screen("display2", size=(1280, 720), scale=1.25)
        res.check("a display grows a screen named after its output id, seeded with its size",
                  ("add", SESSION, "SELKIES-2", 1280, 720, 1.25) in pf.calls, pf.calls)
        res.check("and the handler records the screen it grew",
                  handler._session_screens == {"display2": "SELKIES-2"},
                  getattr(handler, "_session_screens", None))

        pf.calls.clear()
        await handler.ensure_session_screen("display2", size=(1280, 720))
        res.check("a display already holding a screen grows no second one",
                  not any(c[0] == "add" for c in pf.calls), pf.calls)
        await handler.ensure_session_screen("primary", size=(1280, 720))
        res.check("nor does the primary, whose screen the session booted with",
                  not any(c[0] == "add" for c in pf.calls), pf.calls)

        positions = await handler._session_screen_positions(["primary", "display2"])
        res.check("the grown screen is addressed by its place among the session's screens",
                  positions == {"primary": 0, "display2": 1}, positions)

        pf.calls.clear()
        await handler.ensure_session_screens(["display2"])
        res.check("a display still attached keeps its screen",
                  not any(c[0] == "remove" for c in pf.calls), pf.calls)

        pf.calls.clear()
        await handler.ensure_session_screens([])
        res.check("a departed display's screen is closed through the rung that grew it",
                  ("remove", "SELKIES-2") in pf.calls and not handler._session_screens,
                  (pf.calls, handler._session_screens))
        res.check("and the session is back to the screen it booted with",
                  [s[0] for s in pf.screens] == ["WL-1"], pf.screens)

        pf.fail_add = True
        pf.calls.clear()
        await handler.ensure_session_screen("display3", size=(800, 600))
        res.check("a refused virtual output leaves the display without a screen, and says so",
                  not handler._session_screens and
                  any("could not add a screen for 'display3'" in line for line in log.lines),
                  (handler._session_screens, log.lines[-1:]))

        pf = FakePixelflux(kde=False)
        pf.screens.append(("WL-2", 1920, 0, 1920, 1080))
        handler = make_handler(pf)
        ok, why = await handler.probe_session_screen_capability()
        res.check("a session without the protocol falls back to its spare screens",
                  ok and not handler.session_screen_control_available()
                  and handler._session_screen_count == 2, (ok, why, pf.calls))
    finally:
        logger.removeHandler(log)


def main() -> "H.Results":
    res = H.Results("session-screen-kwin")
    asyncio.run(scenario(res))
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
