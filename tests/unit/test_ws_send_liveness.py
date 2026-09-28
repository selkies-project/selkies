#!/usr/bin/env python3
"""A slow socket is kept and a dead one is dropped, on the video relay, the
shared audio fan-out, and the control messages alike.

A websockets send waits while its socket's buffers stay full, and over a far
or slow path a key frame takes seconds to drain; the page answers a dropped
socket with a reload, so a send's liveness is judged by whether the socket
drains, never by how long the send took. Driven with stand-in sockets whose
far side takes bytes at a set rate (none at all for a dead one): a send waits
on its drain the way aiohttp's does, and an abort releases it the way a lost
connection does. The shared audio fan-out must also never hold one client's
audio behind another client's drain.
"""
import asyncio
import os
import struct
import sys
import time
from collections import OrderedDict

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies import websockets_mode as w
from selkies.websockets_mode import DataStreamingServer, _VideoRelay

res = H.Results("ws-send-liveness")
HIGH, LOW = 64 * 1024, 16 * 1024


class Sock:
    """The kernel's view: TCP_INFO with the bytes the peer acknowledged."""

    def __init__(self, ws: "Socket") -> None:
        self.ws = ws

    def getsockopt(self, level: int, option: int, size: int) -> bytes:
        return bytes(120) + struct.pack("Q", self.ws.acked)


class Transport:
    def __init__(self, ws: "Socket") -> None:
        self.ws = ws
        self.aborted = False
        self.sock = Sock(ws)

    def get_extra_info(self, name: str):
        return self.sock if name == "socket" else None

    def get_write_buffer_size(self) -> int:
        return self.ws.pending

    def is_closing(self) -> bool:
        return self.aborted

    def abort(self) -> None:
        self.aborted = True
        self.ws.release()


class Socket:
    """A client websocket whose far side takes `rate` bytes a second.

    A send writes the whole frame, then waits while more than the high-water
    mark stands unsent, until the far side has taken it back under the
    low-water mark: aiohttp's flow control, one drain future shared by every
    sender on the socket.
    """

    def __init__(self, rate: float, pending: int = 0) -> None:
        self.rate = rate
        self.pending = pending
        self.acked = 0
        self.writes = []
        self.closed = False
        self.drain = None
        self.transport = Transport(self)
        self._writer = type("Writer", (), {"transport": self.transport})()
        self._task = None

    def start(self) -> None:
        self._task = asyncio.ensure_future(self._far_side())

    async def _far_side(self) -> None:
        while True:
            await asyncio.sleep(0.01)
            take = min(self.pending, int(self.rate * 0.01))
            self.pending -= take
            self.acked += take
            if self.pending <= LOW:
                self.release()

    def release(self) -> None:
        if self.drain is not None and not self.drain.done():
            self.drain.set_result(None)

    async def send_bytes(self, data) -> None:
        if self.transport.aborted:
            raise ConnectionResetError("Cannot write to closing transport")
        self.writes.append((time.monotonic(), bytes(data[:4]), len(data)))
        self.pending += len(data)
        if self.pending > HIGH:
            if self.drain is None or self.drain.done():
                self.drain = asyncio.get_running_loop().create_future()
            await self.drain

    async def send_str(self, data: str) -> None:
        await self.send_bytes(data.encode())

    async def close(self) -> None:
        self.closed = True
        self.transport.abort()

    def stop(self) -> None:
        if self._task:
            self._task.cancel()


def video_frame(fid: int, size: int, key: bool) -> dict:
    head = bytes([0x04, 0x01 if key else 0x00, fid >> 8, fid & 0xFF, 0, 0, 0, 0, 0, 0, 0, 0])
    data = head + bytes(size - len(head))
    return {"data": memoryview(data), "owner": data, "frame_id": fid}


def relay_server(ws: Socket) -> DataStreamingServer:
    server = DataStreamingServer.__new__(DataStreamingServer)
    server.clients = {ws}
    server.display_clients = {"primary": {"ws": ws, "sent_timestamps": OrderedDict(), "sent_bytes": 0}}
    server.video_relay_groups = {"primary": {}}
    server._downlinks = {}
    return server


async def relay_run(rate: float, seconds: float) -> dict:
    """A 600 KB key frame, then a delta frame a second, into one relay."""
    ws = Socket(rate)
    ws.start()
    server = relay_server(ws)
    relay = _VideoRelay(server, "primary", ws, 8 * 1024 * 1024)
    server.video_relay_groups["primary"][ws] = relay
    relay.start()
    start = time.monotonic()
    relay.offer(video_frame(1, 600 * 1024, True))
    fid = 1
    while time.monotonic() - start < seconds:
        await asyncio.sleep(1.0)
        fid += 1
        relay.offer(video_frame(fid, 4 * 1024, False))
    await asyncio.sleep(0.2)
    out = {"kept": ws in server.clients, "aborted": ws.transport.aborted or ws.closed,
           "frames": len(ws.writes), "elapsed": time.monotonic() - start}
    relay.stop()
    ws.stop()
    await asyncio.sleep(0.05)
    return out


async def relay_until_dropped(rate: float, limit: float) -> float:
    """Seconds until a relay sending a 600 KB key frame drops its client."""
    ws = Socket(rate)
    ws.start()
    server = relay_server(ws)
    relay = _VideoRelay(server, "primary", ws, 8 * 1024 * 1024)
    server.video_relay_groups["primary"][ws] = relay
    relay.start()
    start = time.monotonic()
    relay.offer(video_frame(1, 600 * 1024, True))
    while ws in server.clients and time.monotonic() - start < limit:
        await asyncio.sleep(0.05)
    took = time.monotonic() - start
    relay.stop()
    ws.stop()
    await asyncio.sleep(0.05)
    return took if ws not in server.clients else -1.0


async def audio_run(slow: Socket, seconds: float) -> dict:
    """Twenty-millisecond audio chunks to a fast client and `slow` through the shared fan-out."""
    fast = Socket(10 * 1024 * 1024)
    fast.start()
    slow.start()
    server = DataStreamingServer.__new__(DataStreamingServer)
    server.clients = {fast, slow}
    server.display_clients = {}
    server._downlinks = {}
    server.pcmflux_audio_queue = asyncio.Queue(maxsize=120)
    task = asyncio.ensure_future(server._pcmflux_send_audio_chunks())
    enqueued = []
    start = time.monotonic()
    n = 0
    while time.monotonic() - start < seconds:
        chunk = bytes([0x01, 0, n >> 8, n & 0xFF]) + bytes(1000)
        enqueued.append(time.monotonic())
        try:
            server.pcmflux_audio_queue.put_nowait({"data": memoryview(chunk), "owner": chunk})
        except asyncio.QueueFull:
            pass
        n += 1
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.3)
    lateness = [t - enqueued[struct.unpack(">H", head[2:4])[0]]
                for t, head, _ in fast.writes if head[0] == 0x01]
    order = [struct.unpack(">H", head[2:4])[0] for _, head, _ in slow.writes if head[0] == 0x01]
    out = {"chunks": n, "fast_got": len(lateness), "fast_late_max": max(lateness, default=None),
           "slow_kept": slow in server.clients, "slow_got": len(order),
           "slow_in_order": order == sorted(order), "slow_aborted": slow.transport.aborted}
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass
    fast.stop()
    slow.stop()
    await asyncio.sleep(0.05)
    return out


async def control_run(sockets: list) -> dict:
    """One watched control broadcast to `sockets`: who is dropped, and when each send returned."""
    for ws in sockets:
        ws.start()
    clients = set(sockets)
    done = {}

    async def timed():
        start = time.monotonic()
        dropped = await w._broadcast_to_clients(clients, "cursor," + "x" * 200, watched=True)
        done["took"] = time.monotonic() - start
        return dropped

    dropped = await timed()
    out = {"dropped": sorted(sockets.index(ws) for ws in dropped), "kept": sorted(sockets.index(ws) for ws in clients),
           "took": done["took"], "aborted": [ws.transport.aborted for ws in sockets]}
    for ws in sockets:
        ws.stop()
    await asyncio.sleep(0.05)
    return out


# A key frame over a slow path: 600 KB at 150 KB/s drains for about 4 s.
r = asyncio.run(relay_run(150 * 1024, 6.0))
res.check("a relay whose socket drains a key frame for 4 s keeps its client", r["kept"] and not r["aborted"],
          r)
res.check("and goes on sending after it", r["frames"] >= 3, f"{r['frames']} frames written")

# A socket that takes nothing is still dropped.
w.SEND_STALL_SECONDS, w.SEND_PROBE_SECONDS = 1.5, 0.1
took = asyncio.run(relay_until_dropped(0.0, 8.0))
res.check("a relay whose socket takes nothing drops its client", 0.0 < took < 3.0, f"after {took:.2f} s")
# Judged by progress, not by time: with the same short bound, a slow socket outlasts it.
r = asyncio.run(relay_run(150 * 1024, 5.0))
res.check("a socket that keeps draining outlasts a stall bound shorter than its drain", r["kept"],
          f"kept after {r['elapsed']:.1f} s with a {w.SEND_STALL_SECONDS} s bound")
w.SEND_STALL_SECONDS, w.SEND_PROBE_SECONDS = 10.0, 0.5

# The shared audio fan-out: one client behind a 600 KB backlog at 150 KB/s.
a = asyncio.run(audio_run(Socket(150 * 1024, pending=600 * 1024), 3.0))
res.check("a slow client's audio is never dropped for its drain", a["slow_kept"] and not a["slow_aborted"], a)
res.check("and reaches it in order", a["slow_in_order"] and a["slow_got"] >= a["chunks"] - 2,
          f"{a['slow_got']}/{a['chunks']} chunks")
res.check("the other client's audio is not held behind that drain",
          a["fast_got"] >= a["chunks"] - 2 and a["fast_late_max"] is not None and a["fast_late_max"] < 0.1,
          f"{a['fast_got']}/{a['chunks']} chunks, latest {a['fast_late_max'] and round(a['fast_late_max'], 3)} s after its turn")

# A control message queued behind that key frame waits for it, and the client stays.
c = asyncio.run(control_run([Socket(150 * 1024, pending=600 * 1024)]))
res.check("a control send behind a 4 s drain keeps its client", c["kept"] == [0] and not any(c["aborted"]),
          f"returned after {c['took']:.1f} s, {c}")
c = asyncio.run(control_run([Socket(150 * 1024, pending=600 * 1024), Socket(10 * 1024 * 1024)]))
res.check("and every client of the broadcast stays", c["kept"] == [0, 1] and not any(c["aborted"]), c)

w.SEND_STALL_SECONDS, w.SEND_PROBE_SECONDS = 1.5, 0.1
c = asyncio.run(control_run([Socket(0.0, pending=600 * 1024)]))
res.check("a control send to a socket that takes nothing drops it", c["dropped"] == [0] and c["aborted"][0]
          and c["took"] < 3.0, c)
c = asyncio.run(control_run([Socket(0.0, pending=600 * 1024), Socket(10 * 1024 * 1024)]))
res.check("and drops only it from a broadcast", c["dropped"] == [0] and c["kept"] == [1], c)
a = asyncio.run(audio_run(Socket(0.0, pending=600 * 1024), 3.5))
res.check("a dead client is dropped from the audio fan-out", not a["slow_kept"], a)
res.check("while the other client's audio keeps its cadence",
          a["fast_got"] >= a["chunks"] - 2 and a["fast_late_max"] is not None and a["fast_late_max"] < 0.1,
          f"latest {a['fast_late_max'] and round(a['fast_late_max'], 3)} s after its turn")
w.SEND_STALL_SECONDS, w.SEND_PROBE_SECONDS = 10.0, 0.5

sys.exit(0 if res.summary() else 1)
