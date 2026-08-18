#!/usr/bin/env python3
"""Link shaper relay: LISTEN:port -> TARGET:port with a token-bucket rate cap
on the target->client direction (the downlink), so a localhost test path
feels a real bottleneck link exactly where a home connection would. An
optional second rate shapes the client->target direction (the uplink) the
same way, for upload scenarios; without it that direction passes unshaped.
Also emits a throughput tick per direction.

Usage: bwrelay.py LISTEN_PORT TARGET_PORT RATE_KBIT_S [UPLINK_RATE_KBIT_S]
"""
import asyncio
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
TARGET_RCVBUF = 128 * 1024


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


async def pump(reader: asyncio.StreamReader, writer: asyncio.StreamWriter,
               bucket: Bucket, stats: dict) -> None:
    try:
        while True:
            data = await reader.read(CHUNK)
            if not data:
                break
            for off in range(0, len(data), GRAIN):
                piece = data[off:off + GRAIN]
                await bucket.take(len(piece))
                writer.write(piece)
                stats["bytes"] += len(piece)
                await writer.drain()
    except (ConnectionResetError, asyncio.CancelledError, BrokenPipeError):
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def handle(client_r: asyncio.StreamReader, client_w: asyncio.StreamWriter,
                 target_port: int, bucket: Bucket, up_bucket: Optional[Bucket],
                 stats: dict, up_stats: dict) -> None:
    try:
        raw = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        raw.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, TARGET_RCVBUF)
        raw.setblocking(False)
        await asyncio.get_running_loop().sock_connect(raw, ("127.0.0.1", target_port))
        target_r, target_w = await asyncio.open_connection(sock=raw)
    except (ConnectionRefusedError, OSError):
        client_w.close()
        return

    if up_bucket is not None:
        # Bound the client-facing receive buffer like the target-facing one:
        # an uplink's standing queue must stand in the modeled path, not hide
        # in an autotuned relay buffer the sender never feels.
        csock = client_w.get_extra_info("socket")
        if csock is not None:
            csock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, TARGET_RCVBUF)
        up = pump(client_r, target_w, up_bucket, up_stats)
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
        up = passthrough()

    await asyncio.gather(up, pump(target_r, client_w, bucket, stats))


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

    # One shared bucket per direction across ALL connections: the relay is the
    # bottleneck link itself, so every byte of a direction competes for the
    # same allowance.
    bucket = Bucket(rate_bps)
    up_bucket = Bucket(up_rate_kbit * 1000 / 8) if up_rate_kbit else None
    server = await asyncio.start_server(
        lambda r, w: handle(r, w, target_port, bucket, up_bucket, stats, up_stats),
        "127.0.0.1", listen_port)
    up_note = f", uplink cap {up_rate_kbit} kbit/s" if up_rate_kbit else ""
    print(f"[relay] :{listen_port} -> :{target_port} downlink cap {rate_kbit} kbit/s{up_note}", flush=True)
    asyncio.ensure_future(tick())
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main(int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]),
                     int(sys.argv[4]) if len(sys.argv) > 4 else 0))
