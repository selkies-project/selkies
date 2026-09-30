#!/usr/bin/env python3
"""A display's capture runs no faster than its client's decoder keeps up with.

A page whose decoder falls behind the stream tells the server the rate it keeps
up with (`DECODE_PACE`), and the display's capture runs at the lower of that and
the rate the client chose, which stays stored as chosen. Lifting the pace, or a
new connection taking the display over, puts the chosen rate back on the live
capture, so a slow client's pace never outlives its socket.
"""
import os
import sys
import tempfile
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
os.environ["SELKIES_FILE_MANAGER_PATH"] = tempfile.mkdtemp(prefix="selkies-pace-")

import helpers as H  # noqa: E402

import selkies.websockets_mode as S  # noqa: E402


class FakeCapture:
    """A ScreenCapture stand-in recording the rates it is given."""

    def __init__(self) -> None:
        self.rates = []

    def update_framerate(self, fps: float) -> None:
        self.rates.append(fps)


def make_server():
    srv = S.DataStreamingServer.__new__(S.DataStreamingServer)
    srv.cli_args = S.settings
    srv.app = SimpleNamespace(framerate=60.0)
    module = FakeCapture()
    srv.capture_instances = {"primary": {"module": module}}
    srv._starting_captures = {}
    srv.display_clients = {"primary": {"framerate": 60.0, "decode_pace": None}}
    return srv, module


def main() -> int:
    res = H.Results("ws-decode-pace")
    srv, module = make_server()
    state = srv.display_clients["primary"]
    res.check("an unpaced display captures at the rate its client chose",
              srv._capture_fps(state) == 60.0, srv._capture_fps(state))

    srv._apply_decode_pace("primary", state, 41.0)
    res.check("a pace holds the live capture to it", module.rates == [41.0] and srv._capture_fps(state) == 41.0,
              module.rates)
    res.check("and leaves the chosen rate stored", state["framerate"] == 60.0, state["framerate"])

    srv._apply_decode_pace("primary", state, 41.0)
    res.check("the same pace again changes nothing", module.rates == [41.0], module.rates)

    state["framerate"] = 30.0
    res.check("a chosen rate under the pace is taken as chosen", srv._capture_fps(state) == 30.0,
              srv._capture_fps(state))
    state["framerate"] = 60.0

    srv._apply_decode_pace("primary", state, None)
    res.check("lifting the pace puts the chosen rate back", module.rates == [41.0, 60.0] and state["decode_pace"] is None,
              module.rates)

    srv._apply_decode_pace("primary", state, 36.0)
    # What both takeover paths do for the connection that claims the entry.
    srv._apply_decode_pace("primary", state, None)
    res.check("a new connection starts the display unpaced", module.rates[-1] == 60.0 and srv._capture_fps(state) == 60.0,
              module.rates)

    idle, _ = make_server()
    idle.capture_instances = {}
    idle_state = idle.display_clients["primary"]
    idle._apply_decode_pace("primary", idle_state, 25.0)
    res.check("a display with no capture running keeps the pace for its next start",
              idle._capture_fps(idle_state) == 25.0, idle._capture_fps(idle_state))
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
