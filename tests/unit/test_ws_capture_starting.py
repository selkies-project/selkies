#!/usr/bin/env python3
"""A capture that is still starting takes the live requests a running one does.

WebSockets mode registers a display's capture once its start returns, and a
start takes 20-100 ms. A bitrate or frame rate the page set in that window used
to be stored for the display but reached no encoder until the next change,
and a key-frame request or a lost frame's invalidation went nowhere. The
starting module takes them (pixelflux accepts requests while it starts), and
the settings registered once it runs carry what was applied, so a later
reconfigure does not revert it.
"""
import asyncio
import os
import sys
import tempfile
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
os.environ["SELKIES_FILE_MANAGER_PATH"] = tempfile.mkdtemp(prefix="selkies-starting-")

import helpers as H  # noqa: E402

import selkies.websockets_mode as S  # noqa: E402


class FakeCapture:
    """A ScreenCapture stand-in recording the live requests it is given."""

    def __init__(self) -> None:
        self.calls = []

    def update_video_bitrate(self, kbps: int) -> None:
        self.calls.append(("bitrate", kbps))

    def update_framerate(self, fps: float) -> None:
        self.calls.append(("framerate", fps))

    def request_idr_frame(self) -> None:
        self.calls.append(("idr",))

    def invalidate_reference(self, frame_id: int) -> None:
        self.calls.append(("invalidate", frame_id))


def make_server():
    """A DataStreamingServer whose primary capture is mid-start."""
    srv = S.DataStreamingServer.__new__(S.DataStreamingServer)
    srv.cli_args = S.settings
    srv.app = SimpleNamespace(set_framerate=lambda fps: None, framerate=60.0)
    srv.display_clients = {"primary": {"video_bitrate": 8000.0, "framerate": 60.0}}
    srv.display_layouts = {}
    srv.capture_instances = {}
    srv._initial_video_bitrate = 8000.0
    srv._invalidation_log = {}
    module = FakeCapture()
    starting = {"module": module, "settings": SimpleNamespace(video_bitrate_kbps=8000, target_fps=60.0)}
    srv._starting_captures = {"primary": starting}
    return srv, module, starting


def main() -> int:
    res = H.Results("ws-capture-starting")
    srv, module, starting = make_server()
    asyncio.run(srv._handle_opcode_video_bitrate(4000, "primary"))
    res.check("a bitrate set while the capture starts reaches its encoder",
              ("bitrate", 4000) in module.calls, module.calls)
    res.check("and is what the capture is registered with",
              starting["settings"].video_bitrate_kbps == 4000, vars(starting["settings"]))
    asyncio.run(srv._handle_opcode_fps(30, "primary"))
    res.check("so is a frame rate", ("framerate", 30.0) in module.calls and starting["settings"].target_fps == 30.0,
              module.calls)
    srv._schedule_idr_for_display("primary")
    srv._schedule_invalidation("primary", 70000)
    res.check("a key-frame request and a lost frame reach it too",
              ("idr",) in module.calls and ("invalidate", 70000 & 0xFFFF) in module.calls, module.calls)
    srv._starting_captures = {}
    before = len(module.calls)
    asyncio.run(srv._handle_opcode_video_bitrate(3000, "primary"))
    res.check("with no capture running or starting, the rate is stored and nothing is called",
              srv.display_clients["primary"]["video_bitrate"] == 3000 and len(module.calls) == before,
              srv.display_clients["primary"])
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
