#!/usr/bin/env python3
"""What a WebRTC hello may carry and name.

A hello is read before its credential is checked, on the loop every client
shares, so one past a few pages is refused unread. Its display id is one field
of the space-separated SESSION_START line and names takeover-storm keys, so it
must be a short name of letters, digits, `-` and `_`; anything else is refused.
The storm table forgets an identity once its takeovers leave the window, so
hellos naming new displays cannot grow it. Runs the peer registry with stand-in sockets; no server, no browser.
"""
import asyncio
import json
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from aiohttp import WSMsgType
from selkies.webrtc_signaling_server import HELLO_MAX_BYTES, WebRTCPeerManagement


class FakeWs:
    """A signaling socket that records its close and answers one hello."""

    def __init__(self, hello: str = "") -> None:
        self.closed = False
        self.close_args = None
        self.sent: list = []
        self._hello = hello

    async def receive(self) -> SimpleNamespace:
        return SimpleNamespace(type=WSMsgType.TEXT, data=self._hello)

    async def send_str(self, text: str) -> None:
        self.sent.append(text)

    async def close(self, code: int = 1000, message: bytes = b"") -> None:
        self.closed = True
        self.close_args = (code, message)


def registry(sharing: bool) -> WebRTCPeerManagement:
    return WebRTCPeerManagement(SimpleNamespace(
        keepalive_timeout=30, turn_shared_secret=None, turn_host=None, turn_port=None,
        turn_protocol="udp", turn_tls=False, turn_auth_header_name="x-auth-user",
        stun_host=None, stun_port=None, enable_sharing=sharing, enable_shared=True,
        enable_player2=False, enable_player3=False, enable_player4=False,
        rtc_config="{}", rtc_config_file=os.devnull))


def hello(client_type: str, **meta) -> str:
    return "HELLO client " + json.dumps(dict(client_type=client_type, client_slot=1, **meta))


async def join(peers: WebRTCPeerManagement, text: str, ceiling: str = None) -> FakeWs:
    ws = FakeWs(text)
    try:
        await peers.hello_peer(ws, "page", ceiling)
    except Exception:
        pass
    return ws


async def scenario(res: H.Results) -> None:
    peers = registry(sharing=True)
    big = await join(peers, hello("controller", pad="x" * HELLO_MAX_BYTES))
    res.check("a hello past HELLO_MAX_BYTES is refused unread",
              big.close_args is not None and big.close_args[0] == 1009 and not peers.peers, big.close_args)
    for bad in ("primary 0 right forged-token", "x" * 33, "", 7, ["primary"], {"a": 1}):
        ws = await join(peers, hello("controller", display_id=bad))
        refused = ws.close_args is not None and ws.close_args[0] == 1002
        res.check(f"display_id {bad!r} is {'taken as primary' if bad == '' else 'refused'}",
                  (not ws.closed) if bad == "" else refused, ws.close_args)
        for pid in list(peers.peers):
            peers.peers.pop(pid, None)
    ok = await join(peers, hello("controller", display_id="display2"))
    res.check("display_id display2 is taken", not ok.closed, ok.close_args)

    peers = registry(sharing=False)
    peers._EVICTION_STORM_WINDOW_S = 0.05
    for i in range(50):
        await join(peers, hello("controller", display_id=f"d{i}"))
        await join(peers, hello("controller", display_id=f"d{i}"))
    await asyncio.sleep(0.1)
    await join(peers, hello("controller", display_id="last"))
    await join(peers, hello("controller", display_id="last"))
    res.check("the storm table forgets identities whose takeovers left the window",
              len(peers._eviction_times) == 1, len(peers._eviction_times))


def main() -> bool:
    res = H.Results("webrtc-hello-bounds")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
