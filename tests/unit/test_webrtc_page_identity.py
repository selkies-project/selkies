#!/usr/bin/env python3
"""Which WebRTC page a new one supersedes on a display.

Without a master token a page names its player slot in its hello (`#playerN`,
slot 1 when its URL names none) and is the only page of that slot: a newer one
supersedes it, which is how a refreshed page takes its slot back before the old
socket is reaped. With one, the token table says who a page is and which slots
it drives, and every page opened without `#playerN` names slot 1, so the slot
it names says nothing: a collaboration room's controller and participants each
open the session that way, and would take the slot from each other in turn
until the takeover-storm breaker refused one. Such a page supersedes only a page
holding its own token. Runs the peer registry with stand-in sockets; no server,
no browser.
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
from selkies import sessions
from selkies.settings import settings as app_settings
from selkies.webrtc_signaling_server import WebRTCPeerManagement

MASTER = "unit-master"
TOKENS = {"ctl-Rk2": {"role": "controller", "slot": None},
          "ann-Hq7": {"role": "viewer", "slot": [3, 4]},
          "bo-Zt9": {"role": "viewer", "slot": 1}}


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


def hello(client_type: str, token: str = None, tab: str = None, slot: int = 1) -> str:
    meta = {"client_type": client_type, "client_slot": slot}
    if token:
        meta["client_token"] = token
    if tab:
        meta["client_tab_id"] = tab
    return "HELLO client " + json.dumps(meta)


async def join(peers: WebRTCPeerManagement, text: str) -> FakeWs:
    ws = FakeWs(text)
    try:
        await peers.hello_peer(ws, "page", None)
    except Exception:
        pass
    return ws


async def scenario(res: H.Results) -> None:
    saved = (app_settings.master_token, sessions.user_tokens)
    try:
        app_settings.master_token = MASTER
        sessions.user_tokens = dict(TOKENS)
        peers = registry()
        ctl = await join(peers, hello("controller", "ctl-Rk2", "tab-c"))
        ann = await join(peers, hello("controller", "ann-Hq7", "tab-a"))
        bo = await join(peers, hello("controller", "bo-Zt9", "tab-b"))
        res.check("with tokens, a controller and two participants opened without #playerN all stay",
                  not ctl.closed and not ann.closed and not bo.closed and len(peers.peers) == 3,
                  [ctl.close_args, ann.close_args, bo.close_args])
        again = await join(peers, hello("controller", "ann-Hq7", "tab-a"))
        res.check("a page reconnecting with its own token supersedes its old page alone",
                  ann.close_args is not None and ann.close_args[0] == 4001 and not again.closed
                  and not ctl.closed and not bo.closed and len(peers.peers) == 3,
                  [ann.close_args, again.close_args, ctl.close_args, bo.close_args])
        churn = [await join(peers, hello("controller", "bo-Zt9", f"tab-{i}")) for i in range(6)]
        res.check("one token's pages trading the display trip the storm breaker, and the others stay",
                  any(w.close_args and w.close_args[0] == 4000 for w in churn)
                  and not ctl.closed and not again.closed,
                  [w.close_args for w in churn])
    finally:
        app_settings.master_token, sessions.user_tokens = saved

    peers = registry()
    first = await join(peers, hello("viewer", tab="tab-1", slot=1))
    second = await join(peers, hello("viewer", tab="tab-2", slot=1))
    res.check("without a master token, a newer page naming the same slot supersedes the old one",
              first.close_args is not None and first.close_args[0] == 4001 and not second.closed,
              first.close_args)


def main() -> bool:
    res = H.Results("webrtc-page-identity")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
