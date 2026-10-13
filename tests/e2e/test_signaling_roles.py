#!/usr/bin/env python3
"""Who may hold the WebRTC signaling roles, outside secure mode too.

The streaming server's own signaling client holds the server role with a proof
of it; a socket that says HELLO server without one is closed and the real
server peer stays registered. A page's SESSION only ever calls the server: one
naming another page is refused, and that page hears nothing of it, while the
same page calling the server is answered. Raw signaling sockets against a
WebRTC server, no browser.

Usage: python3 tests/e2e/test_signaling_roles.py
"""
import asyncio
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets

URI = f"ws://localhost:{H.PORT}/api/ws"


def registered(peer_type: str) -> list:
    return re.findall(rf"Registered peer ({peer_type}-[0-9a-f-]+)", H.server_log())


async def hello(peer_type: str, meta: dict) -> websockets.ClientConnection:
    ws = await websockets.connect(URI)
    await ws.send(f"HELLO {peer_type} {json.dumps(meta)}")
    await asyncio.wait_for(ws.recv(), 5)
    return ws


async def quiet(ws, seconds: float) -> list:
    heard = []
    try:
        while True:
            heard.append(await asyncio.wait_for(ws.recv(), seconds))
    except (asyncio.TimeoutError, websockets.exceptions.ConnectionClosed):
        pass
    return heard


async def drive(res: "H.Results") -> None:
    deadline = time.time() + 30
    while not registered("server") and time.time() < deadline:
        await asyncio.sleep(0.5)
    servers = registered("server")
    res.check("the streaming server registers its server peer", len(servers) == 1, servers)

    code = None
    try:
        async with websockets.connect(URI) as ws:
            await ws.send("HELLO server")
            await asyncio.wait_for(ws.recv(), 5)
    except websockets.exceptions.ConnectionClosed as e:
        code = e.rcvd.code if e.rcvd else e.code
    res.check("a HELLO server without a proof is closed with 4001", code == 4001, code)
    await asyncio.sleep(1.0)
    res.check("and the streaming server's peer stays the only one", registered("server") == servers,
              registered("server"))

    victim = await hello("client", {"client_type": "controller", "client_slot": -1})
    await asyncio.sleep(0.5)
    victim_uid = registered("client")[-1]
    other = await hello("client", {"client_type": "viewer", "client_slot": -1})
    try:
        await other.send(f"SESSION {victim_uid}")
        reply = await asyncio.wait_for(other.recv(), 5)
        res.check("a SESSION naming another page is refused", reply.startswith("ERROR"), reply)
        heard = await quiet(victim, 1.5)
        res.check("and that page hears nothing of it", not heard, heard[:2])
        await other.send("SESSION server")
        reply = await asyncio.wait_for(other.recv(), 5)
        res.check("the same page calling the server is answered", reply == f"SESSION_OK {servers[0]}", reply)
    finally:
        await other.close()
        await victim.close()


def main() -> bool:
    res = H.Results("signaling-roles")
    H.server_start(mode="webrtc", wayland=False)
    try:
        asyncio.run(drive(res))
    finally:
        H.server_stop()
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
