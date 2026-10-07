#!/usr/bin/env python3
"""A transport endpoint's answer to the page's probe.

Before reconnecting (WebSockets) or reloading (WebRTC), the page sends a plain
GET to its transport's WebSocket route and reads the status: 409 is the other
transport serving, 503 a WebRTC service starting or stopping, and anything else
a server that is up. The route answers that GET 204 when its transport is up,
since an error status there is logged by every browser as a failed load at each
reconnect; an upgrade is still the only request it serves a socket.
"""
import asyncio
import os
import sys
from types import SimpleNamespace

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "src"))

from aiohttp.test_utils import make_mocked_request  # noqa: E402

from selkies import webrtc_mode as rm  # noqa: E402
from selkies import websockets_mode as wm  # noqa: E402

failures = 0


def check(name: str, ok: bool, detail: object = "") -> None:
    global failures
    print(("PASS  " if ok else "FAIL  ") + name + (f"  {detail}" if detail != "" and not ok else ""))
    if not ok:
        failures += 1


async def run() -> None:
    ws = object.__new__(wm.DataStreamingServer)
    ws.mode = "websockets"
    ws.cli_args = SimpleNamespace(master_token="")
    ws.supervisor = SimpleNamespace(current_mode="websockets")
    probe = make_mocked_request("GET", "/api/websockets")
    status = (await ws.data_ws_handler(probe)).status
    check("websockets: the probe of a serving transport is answered 204", status == 204, status)
    ws.supervisor.current_mode = "webrtc"
    status = (await ws.data_ws_handler(probe)).status
    check("websockets: and 409 while WebRTC serves", status == 409, status)

    wr = object.__new__(rm.WebRTCService)
    wr.mode = "webrtc"
    wr.supervisor = SimpleNamespace(current_mode="webrtc")
    wr.peer_manager = object()
    wr._shutdown_called = False
    wr.shutdown_event = asyncio.Event()
    probe = make_mocked_request("GET", "/api/webrtc/signaling")
    status = (await wr.rtc_ws_handler(probe)).status
    check("webrtc: the probe of a serving transport is answered 204", status == 204, status)
    wr.peer_manager = None
    status = (await wr.rtc_ws_handler(probe)).status
    check("webrtc: 503 while the service starts", status == 503, status)
    wr.supervisor.current_mode = "websockets"
    status = (await wr.rtc_ws_handler(probe)).status
    check("webrtc: and 409 while WebSockets serves", status == 409, status)


def main() -> int:
    asyncio.run(run())
    print(f"\n{failures} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
