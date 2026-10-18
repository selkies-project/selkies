#!/usr/bin/env python3
"""A token table grants exactly what it says.

Only the JSON true elects an entry's mk_control, so a provisioning slip such as
the string "false" hands no one the input authority. A new table holds a live
WebRTC peer to its token's role the way both transports' connect paths read
it: an entry that names no role demotes a controller, as it does on
WebSockets, and leaves a viewer that connected on it alone.
"""
import asyncio
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.argv = ["selkies"]

from selkies import sessions  # noqa: E402
from selkies.webrtc_engine import ClientType  # noqa: E402
from selkies.webrtc_mode import WebRTCService  # noqa: E402
from selkies.websockets_mode import DataStreamingServer  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [token-table] {label}  {detail}", flush=True)


def provision(table: dict) -> None:
    ws = DataStreamingServer.__new__(DataStreamingServer)
    ws.config_gate = asyncio.Event()
    ws.provision_tokens(table)


class _App:
    def __init__(self, peers: dict) -> None:
        self.peer_connections = peers
        self.stopped = []

    async def stop_rtc_connection(self, peer_id: str, role: str) -> None:
        self.stopped.append(peer_id)

    def _send_collab_state(self, *args) -> None:
        pass


async def reconcile(peers: dict, table: dict) -> list:
    """The peers a WebRTC reconcile against `table` disconnects."""
    sessions.user_tokens, sessions.active_mk_token = table, None
    service = WebRTCService.__new__(WebRTCService)
    service.rtc_app = _App(peers)
    service.input_handler = None
    closed = []

    async def close(peer_id: str, code: int, message: bytes) -> None:
        closed.append(peer_id)

    service._close_peer_signaling_ws = close
    await service.reconcile_webrtc_peers()
    return closed


def peer(token: str, client_type: ClientType) -> dict:
    return {"client_token": token, "client_type": client_type, "client_slot": None, "data_channel": None}


async def main() -> None:
    for value in (True, "true", "false", 1, "1", "yes", None, {}):
        provision({"view": {"role": "viewer", "slot": None, "mk_control": value}})
        elected = sessions.active_mk_token == "view"
        check(f"mk_control {value!r} {'elects' if value is True else 'elects no one'}",
              elected == (value is True), sessions.active_mk_token)
    provision({"ctrl": {"role": "controller", "slot": None}, "view": {"role": "viewer", "slot": None, "mk_control": True}})
    check("the first true entry holds the input authority", sessions.active_mk_token == "view")

    closed = await reconcile({"ctrl": peer("t-ctrl", ClientType.CONTROLLER),
                              "roleless": peer("t-roleless", ClientType.VIEWER),
                              "kept": peer("t-kept", ClientType.CONTROLLER)},
                             {"t-ctrl": {"slot": None}, "t-roleless": {"slot": None},
                              "t-kept": {"role": "controller", "slot": None}})
    check("a controller whose entry loses its role is disconnected", "ctrl" in closed, closed)
    check("a viewer on a roleless entry stays connected", "roleless" not in closed, closed)
    check("a controller whose entry still says so stays connected", "kept" not in closed, closed)
    closed = await reconcile({"gone": peer("t-gone", ClientType.VIEWER)}, {})
    check("a token the table drops is disconnected", closed == ["gone"], closed)

    sessions.user_tokens, sessions.active_mk_token = {}, None
    print(f"[token-table] {passed}/{passed + failed} passed", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    asyncio.run(main())
