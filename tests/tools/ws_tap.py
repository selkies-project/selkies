#!/usr/bin/env python3
"""A reverse proxy in front of the server under test that logs every text
message the browser sends on its WebSocket, and optionally every request line.

The client's socket lives in its socket worker, which a page-side hook never
sees, so a suite that has to count what the worker sends (frame acks) puts
this in front of the server and reads the log; one that has to know what a
proxy's access log would record also names a request log. HTTP requests are
relayed whole; a WebSocket's subprotocols and frames are relayed as a proxy
relays them, the one the server selects echoed to the browser.

Usage: ws_tap.py <listen_port> <upstream_port> <log_path> [<request_log_path>]
Log lines: `<monotonic seconds> <first 80 characters of the message>`.
Request log lines: `<method> <path and query> [<offered subprotocols>]`, a
session-token subprotocol shown without its value.
"""
import asyncio
import sys
import time

import aiohttp
from aiohttp import web

LISTEN, UPSTREAM, LOG_PATH = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
HOP_HEADERS = {"host", "transfer-encoding", "content-length", "connection", "keep-alive"}
log = open(LOG_PATH, "a", buffering=1)
request_log = open(sys.argv[4], "a", buffering=1) if len(sys.argv) > 4 else None


async def relay_socket(request: web.Request, protocols: list) -> web.StreamResponse:
    """Bridge one client WebSocket to the upstream server, logging client text."""
    async with aiohttp.ClientSession() as session:
        try:
            upstream = await session.ws_connect(f"ws://127.0.0.1:{UPSTREAM}{request.rel_url}",
                                                max_msg_size=0, protocols=protocols)
        except aiohttp.WSServerHandshakeError as e:
            return web.Response(status=e.status)
        ws = web.WebSocketResponse(max_msg_size=0, protocols=(upstream.protocol,) if upstream.protocol else ())
        await ws.prepare(request)
        async with upstream:
            async def client_to_server() -> None:
                async for m in ws:
                    if m.type == aiohttp.WSMsgType.TEXT:
                        log.write(f"{time.monotonic():.3f} {m.data[:80]}\n")
                        await upstream.send_str(m.data)
                    elif m.type == aiohttp.WSMsgType.BINARY:
                        await upstream.send_bytes(m.data)
                    else:
                        break
                await upstream.close()

            async def server_to_client() -> None:
                async for m in upstream:
                    if m.type == aiohttp.WSMsgType.TEXT:
                        await ws.send_str(m.data)
                    elif m.type == aiohttp.WSMsgType.BINARY:
                        await ws.send_bytes(m.data)
                    else:
                        break
                await ws.close()

            await asyncio.gather(client_to_server(), server_to_client(), return_exceptions=True)
    return ws


async def handle(request: web.Request) -> web.StreamResponse:
    upgrade = request.headers.get("Upgrade", "").lower() == "websocket"
    protocols = [p.strip() for h in request.headers.getall("Sec-WebSocket-Protocol", ())
                 for p in h.split(",") if p.strip()] if upgrade else []
    if request_log:
        shown = ",".join("selkies.token.*" if p.startswith("selkies.token.") else p for p in protocols)
        request_log.write(f"{request.method} {request.rel_url} {shown}".rstrip() + "\n")
    if upgrade:
        return await relay_socket(request, protocols)
    body = await request.read()
    headers = {k: v for k, v in request.headers.items() if k.lower() not in HOP_HEADERS}
    async with aiohttp.ClientSession(auto_decompress=False) as session:
        async with session.request(request.method, f"http://127.0.0.1:{UPSTREAM}{request.rel_url}",
                                   headers=headers, data=body, allow_redirects=False) as r:
            data = await r.read()
            out = {k: v for k, v in r.headers.items() if k.lower() not in HOP_HEADERS}
            return web.Response(status=r.status, body=data, headers=out)


app = web.Application(client_max_size=1 << 30)
app.router.add_route("*", "/{tail:.*}", handle)
web.run_app(app, host="127.0.0.1", port=LISTEN, print=None, access_log=None)
