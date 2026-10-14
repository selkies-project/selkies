#!/usr/bin/env python3
"""A WebRTC page's session outliving a cut of its signaling socket.

An ingress that retires its connections cuts the page's signaling socket with
no close frame while the media path, which never needed it, runs on. The
server holds such a page's session for SESSION_HOLD_S: a hello naming the
page's resume key is answered RESUMED, carries on under the old peer id with
what the server peer sent meanwhile, and the server peer is told nothing. A
session nobody resumes ends after the hold; a close frame (a reload, a closed
tab) ends it at once; another page, a wrong key or another role gets a session
of its own as before; a cut that reached the page alone (the server's side
still open) resumes the same way, and the stale socket closing after does not
end it. Runs the peer registry with scripted sockets; no server, no browser.
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
import selkies.webrtc_signaling_server as S

KEY = "0123456789abcdef0123456789abcdef"


class Sock:
    """A signaling socket fed by the test: lines, a cut (the stream ends with
    no close frame), or the page's close frame."""

    made: list = []

    def __init__(self, *lines: str) -> None:
        Sock.made.append(self)
        self.q: asyncio.Queue = asyncio.Queue()
        self.closed = False
        self.close_args = None
        self.sent: list = []
        for line in lines:
            self.feed(line)

    def feed(self, text: str) -> None:
        self.q.put_nowait(SimpleNamespace(type=WSMsgType.TEXT, data=text))

    def cut(self) -> None:
        self.q.put_nowait(SimpleNamespace(type=WSMsgType.CLOSED, data=None))

    def close_frame(self) -> None:
        self.q.put_nowait(SimpleNamespace(type=WSMsgType.CLOSE, data=1001))

    async def receive(self) -> SimpleNamespace:
        msg = await self.q.get()
        if msg.type in (WSMsgType.CLOSED, WSMsgType.CLOSE, WSMsgType.CLOSING):
            self.closed = True
        return msg

    async def send_str(self, text: str) -> None:
        if self.closed:
            raise ConnectionResetError("Cannot write to closing transport")
        self.sent.append(text)

    async def close(self, code: int = 1000, message: bytes = b"") -> None:
        if not self.closed:
            self.close_args = (code, message)
            self.q.put_nowait(SimpleNamespace(type=WSMsgType.CLOSING, data=None))
        self.closed = True

    async def ping(self) -> None:
        pass


def registry(sharing: bool = True) -> S.WebRTCPeerManagement:
    return S.WebRTCPeerManagement(SimpleNamespace(
        keepalive_timeout=30, turn_shared_secret=None, turn_host=None, turn_port=None,
        turn_protocol="udp", turn_tls=False, turn_auth_header_name="x-auth-user",
        stun_host=None, stun_port=None, enable_sharing=sharing, enable_shared=True,
        enable_player2=False, enable_player3=False, enable_player4=False,
        rtc_config="{}", rtc_config_file=os.devnull))


def hello(client_type: str = "controller", key: str = KEY) -> str:
    meta = {"client_type": client_type, "client_slot": -1}
    if key:
        meta["resume_key"] = key
    return "HELLO client " + json.dumps(meta)


async def settle() -> None:
    """Until every scripted socket's lines were read, and what they set off ran."""
    for _ in range(10000):
        await asyncio.sleep(0)
        if all(sock.q.empty() for sock in Sock.made):
            break
    await asyncio.sleep(0.05)


class Rig:
    """A registry with its server peer up and one page in a session."""

    async def start(self, sharing: bool = True, client_type: str = "controller", key: str = KEY) -> "Rig":
        self.peers = registry(sharing)
        self.server = Sock("HELLO server " + json.dumps({"server_token": S.SERVER_PEER_SECRET}))
        self.tasks = [asyncio.ensure_future(self.peers.signaling_handler(self.server, "server"))]
        await settle()
        self.page = await self.join(Sock(hello(client_type, key), "SESSION server"))
        self.uid = self.started()[-1]
        return self

    async def join(self, sock: Sock) -> Sock:
        self.tasks.append(asyncio.ensure_future(self.peers.signaling_handler(sock, "page")))
        await settle()
        return sock

    def started(self) -> list:
        return [line.split()[1] for line in self.server.sent if line.startswith("SESSION_START")]

    def ended(self) -> list:
        return [line.split()[1] for line in self.server.sent if line.startswith("SESSION_END")]

    def stop(self) -> None:
        for task in self.tasks:
            task.cancel()


async def scenario(res: H.Results) -> None:
    S.SESSION_HOLD_S = 1.0

    rig = await Rig().start()
    rig.page.cut()
    await settle()
    rig.server.feed(f'{rig.uid} {{"ice": 1}}')
    rig.server.feed(f'{rig.uid} {{"ice": 2}}')
    await settle()
    res.check("a cut page's session is held: the server peer hears nothing, and lines to it wait",
              rig.ended() == [] and rig.uid in rig.peers.sessions and len(rig.peers.peers[rig.uid].held) == 2,
              {"ended": rig.ended(), "held": rig.peers.peers[rig.uid].held})
    back = await rig.join(Sock(hello()))
    server_id = rig.peers.sessions.get(rig.uid)
    res.check("a hello naming its key resumes it: RESUMED, then the held lines in order, under the old id",
              back.sent == [f"RESUMED {server_id}", f'{server_id} {{"ice": 1}}', f'{server_id} {{"ice": 2}}']
              and rig.peers.peers[rig.uid].ws is back and rig.peers.peers[rig.uid].held == [],
              back.sent)
    await asyncio.sleep(S.SESSION_HOLD_S * 2)
    rig.server.feed(f'{rig.uid} {{"ice": 3}}')
    back.feed(f'{server_id} {{"ice": 4}}')
    await settle()
    res.check("the resumed session outlives the hold and relays both ways, with no second start or end",
              rig.ended() == [] and rig.started() == [rig.uid] and back.sent[-1] == f'{server_id} {{"ice": 3}}'
              and rig.server.sent[-1] == f'{rig.uid} {{"ice": 4}}', {"page": back.sent, "server": rig.server.sent})
    rig.stop()

    rig = await Rig().start()
    rig.page.cut()
    await settle()
    await asyncio.sleep(S.SESSION_HOLD_S * 2)
    res.check("a session nobody resumes ends after the hold",
              rig.ended() == [rig.uid] and rig.uid not in rig.peers.peers, rig.ended())
    late = await rig.join(Sock(hello()))
    res.check("a hello after the hold is a new page: HELLO, a new session",
              late.sent[:1] == ["HELLO"], late.sent)
    rig.stop()

    rig = await Rig().start()
    rig.page.close_frame()
    await settle()
    res.check("a close frame (a reload, a closed tab) ends the session at once",
              rig.ended() == [rig.uid] and rig.uid not in rig.peers.peers, rig.ended())
    rig.stop()

    rig = await Rig().start(sharing=False)
    rig.page.cut()
    await settle()
    other = await rig.join(Sock(hello(key="f" * 32), "SESSION server"))
    res.check("another page (another key) gets a session of its own, and the held one ends",
              other.sent[:1] == ["HELLO"] and rig.ended() == [rig.uid] and len(rig.started()) == 2,
              {"other": other.sent, "ended": rig.ended()})
    rig.stop()

    rig = await Rig().start()
    rig.page.cut()
    await settle()
    viewer = await rig.join(Sock(hello("viewer")))
    res.check("the same key in another role resumes nothing",
              viewer.sent[:1] == ["HELLO"] and rig.peers.peers[rig.uid].ws is rig.page, viewer.sent)
    rig.stop()

    rig = await Rig().start(key="")
    rig.page.cut()
    await settle()
    res.check("a page that named no key (an older client) ends at once on a cut, as before",
              rig.ended() == [rig.uid], rig.ended())
    rig.stop()

    rig = await Rig().start()
    fresh = await rig.join(Sock(hello()))
    await settle()
    res.check("a cut the server did not see: the hello resumes it and the stale socket is closed",
              fresh.sent[:1] == [f"RESUMED {rig.peers.sessions.get(rig.uid)}"] and rig.page.close_args is not None
              and rig.page.close_args[0] == 1000, {"fresh": fresh.sent, "stale": rig.page.close_args})
    await asyncio.sleep(S.SESSION_HOLD_S * 2)
    res.check("and its closing ends nothing: the peer stays on the new socket",
              rig.ended() == [] and rig.peers.peers[rig.uid].ws is fresh, rig.ended())
    rig.stop()

    rig = await Rig().start()
    rig.page.cut()
    await settle()
    for i in range(S.HELD_LINES_MAX + 10):
        rig.server.feed(f'{rig.uid} {{"ice": {i}}}')
    await settle()
    await settle()
    res.check("the server peer relaying to a cut page carries on, and what it holds is bounded",
              not rig.server.closed and len(rig.peers.peers[rig.uid].held) == S.HELD_LINES_MAX,
              len(rig.peers.peers[rig.uid].held))
    rig.stop()


def main() -> bool:
    res = H.Results("webrtc-signaling-resume")
    asyncio.run(scenario(res))
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
