#!/usr/bin/env python3
"""Who owns a display's media over WebRTC, and when it is released.

A display's media graph and capture follow its consumers: a lone viewer of the
primary brings them up (websockets parity), a controller joining reuses them,
an explicit stop releases them in one place without waiting for the peer
connection's state machine, a secondary display goes with its controller, and
a secondary whose controller never connected leaves no phantom registration
behind. Driven against RTCApp with stub callbacks and real (loopback-only)
peer connections; no signaling, no browser.
"""
import asyncio
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.rtc import RTCApp, ClientType


class Service:
    """Records the start/stop_display_media calls an RTCApp makes."""

    def __init__(self) -> None:
        self.started: list = []
        self.stopped: list = []
        self.display_clients: dict = {}

    async def start_display_media(self, display_id: str) -> None:
        self.started.append(display_id)

    async def stop_display_media(self, display_id: str) -> None:
        self.stopped.append(display_id)
        # The owning service unregisters a secondary here.
        if display_id != "primary":
            self.display_clients.pop(display_id, None)


def make_app(loop: asyncio.AbstractEventLoop) -> tuple:
    app = RTCApp(async_event_loop=loop, encoder="h264enc", stun_servers=[], turn_servers=[])
    svc = Service()
    app.start_display_media = svc.start_display_media
    app.stop_display_media = svc.stop_display_media
    app.on_sdp = _no_sdp
    app.on_ice = _no_ice
    return app, svc


async def _no_sdp(sdp_type, sdp, peer_id):
    return None


async def _no_ice(mlineindex, candidate, peer_id):
    return None


async def scenario(res: H.Results) -> None:
    loop = asyncio.get_running_loop()

    # --- a lone viewer of the primary is served -------------------------
    app, svc = make_app(loop)
    await app.start_rtc_connection("v1", "viewer", None, "primary")
    res.check("lone viewer: peer registered", "v1" in app.peer_connections,
              sorted(app.peer_connections))
    res.check("lone viewer: primary graph built", "primary" in app.displays,
              sorted(app.displays))
    await app.on_peer_connection_established("v1", ClientType.VIEWER, "primary")
    res.check("lone viewer: connected viewer starts the display media",
              svc.started == ["primary"], svc.started)

    # --- a controller joining reuses the viewer's graph -----------------
    graph_before = app.displays.get("primary")
    await app.start_rtc_connection("c1", "controller", None, "primary")
    res.check("controller joins: the shared graph is reused",
              app.displays.get("primary") is graph_before and "c1" in app.peer_connections,
              sorted(app.peer_connections))

    # --- the controller leaves: the viewer keeps the primary ------------
    await app.stop_rtc_connection("c1", "controller")
    res.check("controller leaves: explicit stop deregisters it",
              "c1" not in app.peer_connections, sorted(app.peer_connections))
    res.check("controller leaves: the remaining viewer keeps graph and capture",
              "primary" in app.displays and svc.stopped == [], (sorted(app.displays), svc.stopped))

    # --- the last consumer releases the primary explicitly --------------
    await app.stop_rtc_connection("v1", "viewer")
    res.check("last viewer leaves: stop_display_media called from the explicit stop",
              svc.stopped == ["primary"], svc.stopped)
    res.check("last viewer leaves: graph released", app.displays == {}, sorted(app.displays))

    # --- a secondary display goes with its controller ------------------
    app, svc = make_app(loop)
    await app.start_rtc_connection("c1", "controller", None, "primary")
    svc.display_clients["display2"] = {"width": 0, "height": 0}
    await app.start_rtc_connection("c2", "controller", None, "display2")
    await app.start_rtc_connection("v2", "viewer", None, "display2")
    res.check("secondary: controller and viewer attached",
              {"c2", "v2"} <= set(app.peer_connections) and "display2" in app.displays,
              sorted(app.peer_connections))
    await app.stop_rtc_connection("c2", "controller")
    # The orphaned viewer's connection was closed; its state event reaps it.
    await asyncio.sleep(0.2)
    res.check("secondary: controller leaving releases the display",
              svc.stopped == ["display2"] and "display2" not in app.displays,
              (svc.stopped, sorted(app.displays)))
    res.check("secondary: its viewer is closed with it",
              "v2" not in app.peer_connections, sorted(app.peer_connections))
    res.check("secondary: the primary is untouched",
              "c1" in app.peer_connections and "primary" in app.displays, sorted(app.displays))

    # --- a viewer of a secondary without its controller is refused ------
    await app.start_rtc_connection("v3", "viewer", None, "display2")
    res.check("secondary viewer without controller: refused, nothing built",
              "v3" not in app.peer_connections and "display2" not in app.displays,
              (sorted(app.peer_connections), sorted(app.displays)))
    res.check("secondary viewer without controller: no release churn",
              svc.stopped == ["display2"], svc.stopped)

    # --- a failed secondary start leaves no phantom registration --------
    svc.display_clients["display2"] = {"width": 0, "height": 0}

    async def failing_sdp(sdp_type, sdp, peer_id):
        raise RuntimeError("signaling socket gone")

    app.on_sdp = failing_sdp
    await app.start_rtc_connection("c3", "controller", None, "display2")
    res.check("failed secondary start: peer not registered",
              "c3" not in app.peer_connections, sorted(app.peer_connections))
    res.check("failed secondary start: stop_display_media reached (registration dropped)",
              svc.stopped[-1:] == ["display2"] and "display2" not in svc.display_clients,
              (svc.stopped, svc.display_clients))
    res.check("failed secondary start: its graph is gone",
              "display2" not in app.displays, sorted(app.displays))
    res.check("failed secondary start: the primary is untouched",
              "primary" in app.displays and "c1" in app.peer_connections, sorted(app.displays))

    # --- a failed primary controller start beside a viewer keeps the viewer
    app.on_sdp = _no_sdp
    await app.start_rtc_connection("v4", "viewer", None, "primary")
    app.on_sdp = failing_sdp
    stopped_before = list(svc.stopped)
    await app.start_rtc_connection("c4", "controller", None, "primary")
    res.check("failed primary start beside a viewer: graph and capture kept",
              "primary" in app.displays and svc.stopped == stopped_before,
              (sorted(app.displays), svc.stopped))
    app.on_sdp = _no_sdp

    await app.stop_all_rtc_connections()
    res.check("stop_all: every peer and graph released",
              app.peer_connections == {} and app.displays == {},
              (sorted(app.peer_connections), sorted(app.displays)))
    res.check("stop_all: the primary media stopped exactly once",
              svc.stopped.count("primary") == 1, svc.stopped)

    # --- the state-machine path still reaps a peer that closed on its own
    app, svc = make_app(loop)
    await app.start_rtc_connection("c1", "controller", None, "primary")
    pc = app.peer_connections["c1"]["peer_conn"]
    await pc.close()
    await asyncio.sleep(0.2)
    res.check("self-closed peer: reaped by the state handler",
              "c1" not in app.peer_connections and svc.stopped == ["primary"] and app.displays == {},
              (sorted(app.peer_connections), svc.stopped, sorted(app.displays)))


def main() -> bool:
    res = H.Results("rtc-peer-lifecycle")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
