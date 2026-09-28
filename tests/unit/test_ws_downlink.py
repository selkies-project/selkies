#!/usr/bin/env python3
"""A data socket's streamed downlink, served by the real handler.

A socket announces a nonce for its audio, the downlink it names is fetched over
HTTP, and it is pinned that the hello comes at once while the socket's audio
stays on the socket, that the confirmation moves the audio onto the stream in order and
sends the switch mark down the socket, that a nonce serves one fetch of its own
socket only, that a request arriving ahead of its nonce waits for it, that a stream never confirmed is closed, that ending it (by the
client, or by the socket leaving) brings the audio back to the socket, that a
nonce opens only the kind it was announced for and an unknown kind nothing, and
that a reader that falls behind loses the oldest packets rather than holding
the rest.
"""
import asyncio
import os
import struct
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from aiohttp import web  # noqa: E402
from aiohttp.test_utils import TestClient, TestServer  # noqa: E402

import selkies.websockets_mode as wsm  # noqa: E402
from selkies.websockets_mode import DataStreamingServer, _StreamDownlink  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [ws-downlink] {label}  {detail}", flush=True)


class Socket:
    """A data socket's stand-in: what the server sends down it, in order."""

    def __init__(self):
        self.closed = False
        self.sent = []

    async def send_str(self, text):
        self.sent.append(text)


def make_server():
    server = DataStreamingServer.__new__(DataStreamingServer)
    server.mode = "websockets"
    server.supervisor = SimpleNamespace(current_mode="websockets")
    server.clients = set()
    server._downlinks = {}
    server._downlink_nonces = {}
    server._downlink_waiters = {}
    return server


def item(payload: bytes) -> dict:
    return {"data": memoryview(payload), "owner": payload}


async def read_records(resp, count, timeout=2.0):
    """The next `count` records of a stream body, as payload bytes."""
    out, buf = [], b""
    while len(out) < count:
        chunk = await asyncio.wait_for(resp.content.readany(), timeout)
        if not chunk:
            break
        buf += chunk
        while len(buf) >= 4 and len(buf) >= 4 + struct.unpack(">I", buf[:4])[0]:
            n = struct.unpack(">I", buf[:4])[0]
            out.append(buf[4:4 + n])
            buf = buf[4 + n:]
    return out


async def main():
    wsm.DOWNLINK_ANNOUNCE_SECONDS = 0.3
    server = make_server()
    app = web.Application()
    app.router.add_get("/api/downlink/{kind}", server.downlink_handler)
    client = TestClient(TestServer(app))
    await client.start_server()
    try:
        ws, other = Socket(), Socket()
        server.clients |= {ws, other}
        nonce = "0123456789abcdef0123456789abcdef"

        resp = await client.get("/api/downlink/audio", params={"stream": nonce})
        check("a nonce nobody announced serves nothing", resp.status == 404, resp.status)

        await server._on_downlink_verb(ws, "audio," + nonce)
        resp = await client.get("/api/downlink/audio", params={"stream": nonce})
        check("the announced nonce opens its socket's stream, unbuffered by a proxy",
              resp.status == 200 and resp.headers.get("X-Accel-Buffering") == "no", resp.status)
        hello = await read_records(resp, 1)
        check("the hello comes at once", hello == [b""], hello)
        again = await client.get("/api/downlink/audio", params={"stream": nonce})
        check("a nonce serves one fetch", again.status == 404, again.status)

        viewers = {ws, other}
        check("before the confirmation the audio stays on the socket",
              server._downlink_push("audio", viewers, item(b"\x01\x00a")) == set())

        await server._on_downlink_verb(ws, "audio,ok")
        await asyncio.sleep(0.05)
        check("the confirmation sends the switch mark down the socket", ws.sent == ["DOWNLINK,audio,switch"], ws.sent)
        streamed = server._downlink_push("audio", viewers, item(b"\x01\x00b"))
        server._downlink_push("audio", viewers, item(b"\x01\x80"))
        check("after it, the audio goes to the stream of that socket alone", streamed == {ws}, streamed)
        records = await read_records(resp, 2)
        check("records carry the socket's messages in order", records == [b"\x01\x00b", b"\x01\x80"], records)

        await server._on_downlink_verb(ws, "audio,off")
        rest = await asyncio.wait_for(resp.content.read(), 2.0)
        check("'off' ends the stream", rest == b"" and (ws, "audio") not in server._downlinks, rest)
        check("and the audio is back on the socket", server._downlink_push("audio", viewers, item(b"\x01\x00c")) == set())

        nonce2 = "fedcba9876543210fedcba9876543210"
        await server._on_downlink_verb(other, "audio," + nonce2)
        stolen = await client.get("/api/downlink/audio", params={"stream": nonce})
        check("a spent nonce cannot be reused for another socket", stolen.status == 404, stolen.status)
        resp2 = await client.get("/api/downlink/audio", params={"stream": nonce2})
        await read_records(resp2, 1)
        await server._on_downlink_verb(other, "audio,ok")
        server._end_downlink(other)
        rest = await asyncio.wait_for(resp2.content.read(), 2.0)
        check("a socket that leaves ends its stream", rest == b"" and not server._downlinks, rest)

        nonce3 = "00112233445566778899aabbccddeeff"
        early = asyncio.ensure_future(client.get("/api/downlink/audio", params={"stream": nonce3}))
        await asyncio.sleep(0.1)
        await server._on_downlink_verb(other, "audio," + nonce3)
        resp4 = await early
        check("a request that arrives ahead of its nonce waits for it", resp4.status == 200, resp4.status)
        server._end_downlink(other)
        await asyncio.wait_for(resp4.content.read(), 2.0)

        nonce4 = "ffeeddccbbaa99887766554433221100"
        await server._on_downlink_verb(other, "audio," + nonce4)
        wrong = await client.get("/api/downlink/video", params={"stream": nonce4})
        check("an unknown kind opens nothing", wrong.status == 404, wrong.status)
        await server._on_downlink_verb(other, "video," + nonce4)
        check("nor is a nonce for an unknown kind registered",
              server._downlink_nonces.get(nonce4) == (other, "audio"), server._downlink_nonces.get(nonce4))
        server._end_downlink(other)

        old = wsm.DOWNLINK_CONFIRM_SECONDS
        wsm.DOWNLINK_CONFIRM_SECONDS = 0.2
        try:
            await server._on_downlink_verb(ws, "audio," + nonce)
            resp3 = await client.get("/api/downlink/audio", params={"stream": nonce})
            body = await asyncio.wait_for(resp3.content.read(), 2.0)
            check("a stream never confirmed is closed after its hello", body == bytes(4), body)
        finally:
            wsm.DOWNLINK_CONFIRM_SECONDS = old
        check("and leaves no stream behind", not server._downlinks, server._downlinks)
    finally:
        await client.close()

    stream = _StreamDownlink("audio")
    for i in range(wsm.DOWNLINK_QUEUE_ITEMS["audio"] + 3):
        stream.push(item(bytes([1, 0, i])))
    kept = []
    while not stream._queue.empty():
        kept.append(stream._queue.get_nowait()["data"][2])
    check("a reader that falls behind loses the oldest packets",
          kept == list(range(3, wsm.DOWNLINK_QUEUE_ITEMS["audio"] + 3)), kept[:3])


if __name__ == "__main__":
    asyncio.run(main())
    print(f"[ws-downlink] {passed} passed, {failed} failed", flush=True)
    sys.exit(1 if failed else 0)
