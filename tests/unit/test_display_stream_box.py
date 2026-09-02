#!/usr/bin/env python3
"""Where a page draws its stream on the desktop, relayed to its neighbours.

A drag held across two browser windows is placed through the box the page it
crossed onto published, so a `vp` message from one display has to reach the
other displays' layout entries. Both services carry that state, apart from each
other: the websockets one on the display's client record, the WebRTC one in a
map of its own whose roster names the primary implicitly.

What a page publishes is also what a page can abuse -- one box is one broadcast
to every client -- so an impossible box is refused and a page that alternates
two is not allowed to amplify. Drives the message dispatch and both services
directly; no browser, no server, no display.
"""
import asyncio
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.input_handler import WebRTCInput
from selkies.selkies import DataStreamingServer
from selkies.webrtc_mode import WebRTCService

LAYOUTS = {"primary": {"x": 0, "y": 0, "w": 1920, "h": 1080},
           "display2": {"x": 1920, "y": 0, "w": 1280, "h": 720}}


def ws_service() -> tuple:
    """The websockets data server, with only the display state this touches."""
    svc = object.__new__(DataStreamingServer)
    svc.display_clients = {"primary": {}, "display2": {}}
    svc.display_layouts = {k: dict(v) for k, v in LAYOUTS.items()}
    sent: list = []

    async def broadcast() -> None:
        sent.append(svc._display_config_payload())

    svc.broadcast_display_config = broadcast
    return svc, sent


def wr_service() -> tuple:
    """The WebRTC service, whose display map holds the secondary alone."""
    svc = object.__new__(WebRTCService)
    svc.display_clients = {"display2": {}}
    svc.display_layouts = {k: dict(v) for k, v in LAYOUTS.items()}
    svc._client_scales = {}
    svc._client_stream_boxes = {}
    sent: list = []
    svc._broadcast_display_config = lambda: sent.append(svc._display_config_payload())
    return svc, sent


async def dispatch(svc, msg: str, display_id: str) -> None:
    """One client message through the shared dispatch, as both transports do."""
    handler = object.__new__(WebRTCInput)
    handler.data_server_instance = svc
    await handler.on_message(msg, display_id)


def box_of(payload: dict, display_id: str) -> tuple:
    entry = (payload.get("layouts") or {}).get(display_id) or {}
    return tuple(entry.get(k) for k in ("originX", "originY", "scaleX", "scaleY"))


async def scenario(res: "H.Results") -> None:
    for name, make in (("websockets", ws_service), ("webrtc", wr_service)):
        svc, sent = make()
        await dispatch(svc, "vp,1512,-645.5,1.25,2.5", "display2")
        res.check(f"[{name}] a published box reaches the layout the pages read",
                  len(sent) == 1 and box_of(sent[-1], "display2") == (1512.0, -645.5, 1.25, 2.5),
                  sent[-1:] )
        # The primary publishes one too: a drag crossing the other way is
        # placed through it.
        await dispatch(svc, "vp,0,0,2,2", "primary")
        res.check(f"[{name}] the primary's own box is relayed as well",
                  len(sent) == 2 and box_of(sent[-1], "primary") == (0.0, 0.0, 2.0, 2.0),
                  sent[-1:])
        await dispatch(svc, "vp,1512,-645.5,1.25,2.5", "display2")
        res.check(f"[{name}] the same box again announces nothing", len(sent) == 2, len(sent))

        for bad in ("vp,0,0,0,1", "vp,0,0,1,1000", "vp,0,0,nan,1",
                    "vp,1e9,0,1,1", "vp,0,0,1", "vp,a,b,c,d"):
            await dispatch(svc, bad, "display2")
        res.check(f"[{name}] an impossible or malformed box changes nothing",
                  len(sent) == 2 and box_of(sent[-1], "display2") == (1512.0, -645.5, 1.25, 2.5),
                  f"{len(sent)} {box_of(sent[-1], 'display2')}")

        await dispatch(svc, "vp,7,7,1,1", "display3")
        res.check(f"[{name}] a display with no rectangle publishes nothing",
                  len(sent) == 2, len(sent))

        # One broadcast per box is the amplification a page could turn on the
        # session; the page republishes what the layout comes back missing.
        before = len(sent)
        for i in range(20):
            await dispatch(svc, f"vp,{i},0,1,1", "display2")
        res.check(f"[{name}] a page alternating boxes cannot broadcast per message",
                  len(sent) - before <= 1, len(sent) - before)
        burst = len(sent)
        await asyncio.sleep(0.25)
        await dispatch(svc, "vp,99,0,1,1", "display2")
        res.check(f"[{name}] the box that follows the burst still lands",
                  len(sent) == burst + 1 and box_of(sent[-1], "display2") == (99.0, 0.0, 1.0, 1.0),
                  box_of(sent[-1], "display2"))


def main() -> bool:
    res = H.Results("display-stream-box")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
