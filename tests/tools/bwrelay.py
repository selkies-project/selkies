#!/usr/bin/env python3
"""Link shaper relay: LISTEN:port -> TARGET:port with a token-bucket rate cap
on the target->client direction (the downlink), so a localhost test path
feels a real bottleneck link exactly where a home connection would. An
optional second rate shapes the client->target direction (the uplink) the
same way, for upload scenarios; without it that direction passes unshaped.
Also emits a throughput tick per direction.

Each direction is one FIFO queue shared by every connection, drained at the
rate: that is what a metered first hop is, and it is why a bulk transfer
delays everything else going the same way. BWRELAY_RCVBUF sizes that queue —
the default models a shallow hop, and a fat one (a consumer modem holding
seconds of data) is what turns an upload into input lag.

Usage: bwrelay.py LISTEN_PORT TARGET_PORT RATE_KBIT_S [UPLINK_RATE_KBIT_S]
"""
import asyncio
import os
import socket
import sys
import time
from typing import Optional

CHUNK = 65536
# Sub-chunk quantum for bucket grabs: one 64 KiB atomic grab at single-digit
# Mbit/s holds the modeled link for tens of milliseconds, which a real
# packet-granular link never does to a small frame beside a bulk flow.
GRAIN = 8192
# Locked receive buffer toward the target: with the kernel's autotuned rmem
# (megabytes on loopback) the standing queue behind the bucket hides inside
# this socket and no sender-side congestion gauge can see it for tens of
# seconds. A real bottleneck queues in the PATH, so the model bounds it.
# BWRELAY_RCVBUF sizes that modeled queue: a metered first hop with a fat
# buffer holds seconds of data, which is what turns a bulk transfer into
# input lag for everything sharing the direction.
TARGET_RCVBUF = int(os.environ.get("BWRELAY_RCVBUF", 128 * 1024))


class Bucket:
    """Token bucket in bytes with a quarter-second burst allowance.

    Takes are FIFO-serialized (the gate is held through the deficit sleep), so
    the bucket behaves like the single bottleneck link it models: concurrent
    connections interleave in arrival order instead of racing for the balance,
    where the bulk flow's polling cadence starves every small taker."""

    def __init__(self, rate_bps: float) -> None:
        self.rate = rate_bps
        self.cap = rate_bps * 0.25
        self.tokens = self.cap
        self.ts = time.monotonic()
        self._gate = asyncio.Lock()

    def _topup(self) -> None:
        now = time.monotonic()
        self.tokens = min(self.cap, self.tokens + (now - self.ts) * self.rate)
        self.ts = now

    async def take(self, n: int) -> None:
        async with self._gate:
            while True:
                self._topup()
                if self.tokens >= n or (self.tokens > 0 and n <= self.cap):
                    self.tokens -= n
                    if self.tokens < 0:
                        # The deficit is slept off here and repaid by the next
                        # _topup's elapsed-time refill; zeroing it after the
                        # sleep would credit the slept interval twice (double
                        # the delivered rate) and park the balance at <= 0,
                        # where a concurrent small take never sees tokens > 0.
                        await asyncio.sleep(-self.tokens / self.rate)
                    return
                await asyncio.sleep(min((n - self.tokens) / self.rate, 0.01))


class Link:
    """One direction of the modeled link: a FIFO queue drained at the rate.

    Every connection going this way feeds the same queue, so a small request
    issued while a bulk transfer runs waits behind what the transfer already
    queued, exactly as it does at a metered hop. The queue is bounded, and a
    sender that fills it stops being read — which is the back-pressure a real
    buffer applies once it is full.
    """

    def __init__(self, rate_bps: float, depth_bytes: int, stats: dict) -> None:
        self.bucket = Bucket(rate_bps)
        self.stats = stats
        self.queue: asyncio.Queue = asyncio.Queue(
            maxsize=max(1, depth_bytes // GRAIN))
        self._drainer: Optional[asyncio.Task] = None

    def start(self) -> None:
        if self._drainer is None:
            self._drainer = asyncio.ensure_future(self._drain())

    async def _drain(self) -> None:
        while True:
            writer, piece = await self.queue.get()
            await self.bucket.take(len(piece))
            try:
                writer.write(piece)
                self.stats["bytes"] += len(piece)
                await writer.drain()
            except (ConnectionResetError, BrokenPipeError, RuntimeError):
                pass

    async def carry(self, reader: asyncio.StreamReader,
                    writer: asyncio.StreamWriter) -> None:
        """Move one connection's bytes onto this direction until it ends."""
        try:
            while True:
                data = await reader.read(CHUNK)
                if not data:
                    break
                for off in range(0, len(data), GRAIN):
                    await self.queue.put((writer, data[off:off + GRAIN]))
        except (ConnectionResetError, asyncio.CancelledError, BrokenPipeError):
            pass
        finally:
            try:
                writer.close()
            except Exception:
                pass


async def handle(client_r: asyncio.StreamReader, client_w: asyncio.StreamWriter,
                 target_port: int, down: "Link", up: Optional["Link"]) -> None:
    try:
        raw = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        raw.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, TARGET_RCVBUF)
        raw.setblocking(False)
        await asyncio.get_running_loop().sock_connect(raw, ("127.0.0.1", target_port))
        target_r, target_w = await asyncio.open_connection(sock=raw)
    except (ConnectionRefusedError, OSError):
        client_w.close()
        return

    if up is not None:
        # Bound the client-facing receive buffer like the target-facing one:
        # an uplink's standing queue must stand in the modeled path, not hide
        # in an autotuned relay buffer the sender never feels.
        csock = client_w.get_extra_info("socket")
        if csock is not None:
            csock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, TARGET_RCVBUF)
        upward = up.carry(client_r, target_w)
    else:
        async def passthrough():
            try:
                while True:
                    data = await client_r.read(CHUNK)
                    if not data:
                        break
                    target_w.write(data)
                    await target_w.drain()
            except (ConnectionResetError, asyncio.CancelledError, BrokenPipeError):
                pass
            try:
                target_w.close()
            except Exception:
                pass
        upward = passthrough()

    await asyncio.gather(upward, down.carry(target_r, client_w))


async def main(listen_port: int, target_port: int, rate_kbit: int,
               up_rate_kbit: int) -> None:
    rate_bps = rate_kbit * 1000 / 8
    stats = {"bytes": 0}
    up_stats = {"bytes": 0}

    async def tick():
        last = up_last = 0
        while True:
            await asyncio.sleep(5)
            now, up_now = stats["bytes"], up_stats["bytes"]
            up_part = (f" up={(up_now-up_last)*8/5/1e6:.2f} Mbit/s"
                       if up_rate_kbit else "")
            print(f"[relay] {time.strftime('%H:%M:%S')} (now-last)={(now-last)*8/5/1e6:.2f} Mbit/s "
                  f"total={now/1e6:.1f} MB{up_part}", flush=True)
            last, up_last = now, up_now

    # One queue per direction across ALL connections: the relay is the
    # bottleneck link itself, so every byte of a direction waits in the same
    # line for the same allowance.
    down = Link(rate_bps, TARGET_RCVBUF, stats)
    down.start()
    up = None
    if up_rate_kbit:
        up = Link(up_rate_kbit * 1000 / 8, TARGET_RCVBUF, up_stats)
        up.start()
    server = await asyncio.start_server(
        lambda r, w: handle(r, w, target_port, down, up),
        "127.0.0.1", listen_port)
    up_note = f", uplink cap {up_rate_kbit} kbit/s" if up_rate_kbit else ""
    print(f"[relay] :{listen_port} -> :{target_port} downlink cap {rate_kbit} kbit/s{up_note}", flush=True)
    asyncio.ensure_future(tick())
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main(int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]),
                     int(sys.argv[4]) if len(sys.argv) > 4 else 0))
