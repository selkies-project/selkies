#!/usr/bin/env python3
"""A pad handle the application stops reading must neither hold back the
others nor be closed under it.

Every open() of a virtual pad is its own connection, and an application often
holds one it never reads: SDL, Wine, or a launcher probes the node and keeps it
open while the game reads another. The kernel treats such a reader as merely
behind: evdev drops what it queued and joydev replays the pad's state, and the
fd stays open. The server does the same: a connection whose send buffer is
full stops receiving events, its reads deliver what had queued, and once it
has caught up it is handed the pad's current state, so it ends where every
other handle is. The handle being read is never paused for it.

Driven against the interposer servers over real sockets, standing in for the
application's handles: one read and one left unread on each of the js and
evdev nodes.
"""
import asyncio
import os
import shutil
import struct
import sys
import tempfile
from typing import Any, Dict, List, Tuple

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
import selkies.input_handler as ih  # noqa: E402

results: List[tuple] = []

EVENTS = 20000
EVDEV_RECORD = struct.Struct("=qqHHi")
JS_RECORD = struct.Struct("=IhBB")


def check(label: str, ok: Any, detail: Any = "") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [gamepad-slow-reader] {label}  {detail}", flush=True)


class Handle:
    """One application handle on a pad node: the handshake, then a parse of
    what it reads into the last value of every control."""

    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter, is_evdev: bool) -> None:
        self.reader = reader
        self.writer = writer
        self.record = EVDEV_RECORD if is_evdev else JS_RECORD
        self.is_evdev = is_evdev
        self.state: Dict[Tuple[int, int], int] = {}
        self.arrivals: List[float] = []
        self.eof = False
        self.records = 0
        self._pending = b""

    @classmethod
    async def open(cls, path: str, is_evdev: bool) -> "Handle":
        reader, writer = await asyncio.open_unix_connection(path)
        await reader.readexactly(ih.EXPECTED_C_STRUCT_SIZE)
        writer.write(struct.pack("=B", 8))
        await writer.drain()
        return cls(reader, writer, is_evdev)

    def _take(self, data: bytes) -> None:
        self._pending += data
        size = self.record.size
        whole = len(self._pending) - len(self._pending) % size
        for i in range(0, whole, size):
            fields = self.record.unpack_from(self._pending, i)
            self.records += 1
            if self.is_evdev:
                _, _, ev_type, code, value = fields
                if ev_type != ih.EV_SYN:
                    self.state[(ev_type, code)] = value
            else:
                _, value, ev_type, number = fields
                self.state[(ev_type & ~ih.JS_EVENT_INIT, number)] = value
        self._pending = self._pending[whole:]

    async def follow(self) -> None:
        """Read until the connection ends, stamping each arrival."""
        loop = asyncio.get_running_loop()
        while True:
            data = await self.reader.read(65536)
            if not data:
                self.eof = True
                return
            self.arrivals.append(loop.time())
            self._take(data)

    async def catch_up(self, quiet: float = 0.3) -> None:
        """Read until nothing more arrives for `quiet` seconds or the connection ends."""
        while True:
            try:
                data = await asyncio.wait_for(self.reader.read(65536), quiet)
            except asyncio.TimeoutError:
                return
            if not data:
                self.eof = True
                return
            self._take(data)


async def pump(gp: ih.SelkiesGamepad, count: int) -> None:
    """Swing the left stick and press a button in alternation, `count` times,
    letting the event processor run between them as live input would, then
    leave the stick at a value the swing never takes."""
    for n in range(count):
        gp.send_event(0, 1.0 if n % 2 else -1.0, False)
        gp.send_event(0, n % 2, True)
        while gp.events_queue.qsize() > 64:
            await asyncio.sleep(0)
    gp.send_event(0, 0.25, False)
    while not gp.events_queue.empty():
        await asyncio.sleep(0.01)


async def main() -> None:
    where = tempfile.mkdtemp(prefix="slow-reader-")
    gp = ih.SelkiesGamepad(os.path.join(where, "selkies_js0.sock"),
                           os.path.join(where, "selkies_event1000.sock"),
                           asyncio.get_running_loop())
    gp.set_config("Test Pad", 16, 4)
    server = asyncio.ensure_future(gp.run_servers())
    await asyncio.sleep(0.3)
    try:
        js_read = await Handle.open(gp.js_sock_path, False)
        js_idle = await Handle.open(gp.js_sock_path, False)
        ev_read = await Handle.open(gp.evdev_sock_path, True)
        ev_idle = await Handle.open(gp.evdev_sock_path, True)
        await asyncio.sleep(0.2)
        followers = [asyncio.ensure_future(h.follow()) for h in (js_read, ev_read)]

        loop = asyncio.get_running_loop()
        start = loop.time()
        await pump(gp, EVENTS)
        took = loop.time() - start
        await asyncio.sleep(0.3)

        for name, h in (("js", js_read), ("evdev", ev_read)):
            gaps = [b - a for a, b in zip(h.arrivals, h.arrivals[1:])]
            worst = max(gaps, default=0.0)
            check(f"the {name} handle being read is never paused for the unread ones",
                  worst < 0.5, f"longest gap {worst * 1000:.0f} ms over {took:.2f} s, {len(h.arrivals)} reads")

        for name, idle, read in (("js", js_idle, js_read), ("evdev", ev_idle, ev_read)):
            await idle.catch_up()
            check(f"an unread {name} handle stays open", not idle.eof)
            check("and is shed events while unread", idle.records < read.records,
                  f"{idle.records} of {read.records} records")
            check("and once read it ends at the pad's state", idle.state and idle.state == read.state,
                  f"{len(idle.state)} controls, {sum(idle.state[k] != read.state.get(k) for k in idle.state)} differ")

        gp.send_event(1, 0.5, False)
        await asyncio.sleep(0.2)
        for name, idle, read in (("js", js_idle, js_read), ("evdev", ev_idle, ev_read)):
            await idle.catch_up()
            check(f"and then follows the {name} stream live", not idle.eof and idle.state == read.state)

        for f in followers:
            f.cancel()
        for h in (js_read, js_idle, ev_read, ev_idle):
            h.writer.close()
    finally:
        await asyncio.wait_for(gp.close(), 5)
        server.cancel()
        shutil.rmtree(where, ignore_errors=True)


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"[gamepad-slow-reader] {len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
