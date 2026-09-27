#!/usr/bin/env python3
"""What a signaling close tells a WebRTC page: come back, or stay down.

The server's own peer leaving, which is what a stopping or restarting server
and a transport switch do, closes every client as going away (1001), the close
lib/signaling.js reconnects from, and so does a service's shutdown for any page
still registered with it. A verdict on the client keeps a close it
does not retry: 4000 with the reason for a refusal at the handshake (a player
slot the server does not offer), 4001 for a page a newer one superseded. Runs
the peer registry with stand-in sockets; no server, no browser.
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
from selkies.webrtc_signaling_server import Peer, WebRTCPeerManagement


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


def registry() -> WebRTCPeerManagement:
    return WebRTCPeerManagement(SimpleNamespace(
        keepalive_timeout=30, turn_shared_secret=None, turn_host=None, turn_port=None,
        turn_protocol="udp", turn_tls=False, turn_auth_header_name="x-auth-user",
        stun_host=None, stun_port=None, enable_sharing=True, enable_shared=True,
        enable_player2=False, enable_player3=False, enable_player4=False,
        rtc_config="{}", rtc_config_file=os.devnull))


def hello(client_type: str, slot: int = -1) -> str:
    return "HELLO client " + json.dumps({"client_type": client_type, "client_slot": slot})


async def scenario(res: H.Results) -> None:
    peers = registry()
    server = FakeWs()
    peers.peers["server-1"] = Peer(uid="server-1", ws=server, raddr="server", peer_type="server",
                                   client_type=None, client_slot=None, client_strict_viewer=None)
    pages = {}
    for uid, client_type in (("client-a", "controller"), ("client-b", "viewer")):
        pages[uid] = FakeWs()
        peers.peers[uid] = Peer(uid=uid, ws=pages[uid], raddr=uid, peer_type="client",
                                client_type=client_type, client_slot=-1, client_strict_viewer=False)
    await peers.remove_peer("server-1")
    res.check("the server peer leaving closes every page as going away",
              all(ws.close_args and ws.close_args[0] == 1001 for ws in pages.values()),
              {uid: ws.close_args for uid, ws in pages.items()})

    peers = registry()
    late = FakeWs()
    peers.peers["client-late"] = Peer(uid="client-late", ws=late, raddr="late", peer_type="client",
                                      client_type="controller", client_slot=-1, client_strict_viewer=False)
    await peers.close_clients()
    res.check("a page still registered when the service stops is closed as going away too",
              late.close_args is not None and late.close_args[0] == 1001, late.close_args)

    refused = FakeWs(hello("viewer", slot=3))
    try:
        await registry().hello_peer(refused, "page", None)
    except Exception:
        pass
    res.check("a player slot the server does not offer is refused with the verdict and its reason",
              refused.close_args is not None and refused.close_args[0] == 4000
              and b"player" in refused.close_args[1], refused.close_args)

    peers = registry()
    first = FakeWs(hello("controller"))
    await peers.hello_peer(first, "first", None)
    second = FakeWs(hello("controller"))
    await peers.hello_peer(second, "second", None)
    res.check("a page a newer controller superseded is told so, not asked back",
              first.close_args is not None and first.close_args[0] == 4001 and not second.closed,
              first.close_args)


def main() -> bool:
    res = H.Results("webrtc-signaling-close")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
