#!/usr/bin/env python3
"""A WebRTC secondary display is refused or released on the server side.

The #display2 page is gated at session start on the effective second-screen
availability (the websockets engine's SETTINGS gate), with a fatal signaling
verdict rather than a page left on "Connecting"; and a secondary whose peer
connection fails to start is unregistered again, so no phantom display is
advertised to the other pages. Runs the service's session-start path against a
real RTCApp with stub signaling; no browser, no X server.
"""
import asyncio
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies import webrtc_mode
from selkies.rtc import RTCApp


class FakePeerManager:
    def __init__(self) -> None:
        self.lock = asyncio.Lock()
        self.peers: dict = {}


class FakeWs:
    def __init__(self) -> None:
        self.closed = False
        self.close_args = None

    async def close(self, code=1000, message=b"") -> None:
        self.closed = True
        self.close_args = (code, message)


def make_service(second_screen: bool) -> tuple:
    svc = webrtc_mode.WebRTCService(SimpleNamespace(set_clients_present=lambda present: None))
    svc.settings = SimpleNamespace(second_screen=(second_screen, False), wayland_host_display="")
    app = RTCApp(async_event_loop=asyncio.get_running_loop(), encoder="h264enc",
                 stun_servers=[], turn_servers=[])
    svc.rtc_app = app
    app.start_display_media = svc.start_display_media
    app.stop_display_media = svc.stop_display_media
    app.on_ice = _no_ice
    svc.peer_manager = FakePeerManager()
    # The service's settings snapshot: enough for the gate and the seed.
    svc.args = SimpleNamespace(enable_webrtc_statistics=False, **{
        k: None for k in list(svc._VIDEO_SETTING_APPLIERS) + ["force_aligned_resolution"]})
    return svc, app


async def _no_ice(mlineindex, candidate, peer_id):
    return None


async def _no_sdp(sdp_type, sdp, peer_id):
    return None


async def scenario(res: H.Results) -> None:
    # Second screen disabled: the page is refused with the reason.
    svc, app = make_service(second_screen=False)
    ws = FakeWs()
    svc.peer_manager.peers["client-1"] = SimpleNamespace(ws=ws, peer_type="client")
    app.on_sdp = _no_sdp
    await svc.handle_session_start("client-1", "controller", None, "display2", "right")
    res.check("disabled: signaling socket closed with the fatal verdict",
              ws.closed and ws.close_args[0] == 4000
              and b"disabled" in ws.close_args[1], ws.close_args)
    res.check("disabled: no display registered", "display2" not in svc.display_clients,
              sorted(svc.display_clients))
    res.check("disabled: no peer connection started",
              "client-1" not in app.peer_connections and app.displays == {},
              (sorted(app.peer_connections), sorted(app.displays)))

    # Available, but the start fails: no phantom registration.
    svc, app = make_service(second_screen=True)
    ws = FakeWs()
    svc.peer_manager.peers["client-2"] = SimpleNamespace(ws=ws, peer_type="client")
    announced: list = []
    svc._broadcast_display_config = lambda: announced.append(
        ["primary"] + [d for d in svc.display_clients if d != "primary"])

    async def failing_sdp(sdp_type, sdp, peer_id):
        raise RuntimeError("signaling socket gone")

    app.on_sdp = failing_sdp
    await svc.handle_session_start("client-2", "controller", None, "display2", "left")
    res.check("failed start: the secondary is unregistered again",
              "display2" not in svc.display_clients and "display2" not in svc.display_pipelines,
              (sorted(svc.display_clients), sorted(svc.display_pipelines)))
    res.check("failed start: its graph is gone", "display2" not in app.displays, sorted(app.displays))
    res.check("failed start: roster announced without the phantom display",
              announced and announced[-1] == ["primary"], announced)
    res.check("failed start: no verdict sent (the peer simply never connected)",
              not ws.closed, ws.close_args)

    # Available and the start succeeds: the display stays registered.
    svc, app = make_service(second_screen=True)
    app.on_sdp = _no_sdp
    await svc.handle_session_start("client-3", "controller", None, "display2", "up")
    entry = svc.display_clients.get("display2")
    res.check("started: secondary registered with its position and seeded settings",
              entry is not None and entry.get("position") == "up"
              and "client-3" in app.peer_connections, entry)
    await app.stop_all_rtc_connections()


def main() -> bool:
    res = H.Results("webrtc-secondary-gate")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
