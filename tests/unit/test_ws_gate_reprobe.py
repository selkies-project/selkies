#!/usr/bin/env python3
"""A backpressure gate its client's acks no longer move is re-probed, whichever
branch closed it.

The websockets gate closes on a client that has fallen behind by more than it
may, and a gated client is sent nothing. One whose acks keep coming but stop
short of what it was sent (a presenter that stalled, a decoder waiting for the
key frame the gate withholds) therefore never catches up: without a re-probe
the gate holds for as long as the screen changes. Each check here runs the real
backpressure loop against a stand-in fan-out that sends only while the gate is
open, over a stand-in link that delivers frames and pings in the order they
were written, and a client that answers the way the ACK handler records it: a
frozen client costs a key frame per re-probe, not a stream; a client waiting
for a key frame recovers on the first re-probe; a slow client still catching
up is left to catch up; a key frame still crossing a slow path, during which
the acks stand still too, is waited for rather than answered with another;
and a client that went silent stays the stall branch's. A lift asks for one key
frame: the deltas that reach the client's relay before it arrives ask for none.
"""
import asyncio
import logging
import os
import sys
import time
from collections import OrderedDict, deque
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies import websockets_mode as w
from selkies.stream_server import note_pong
from selkies.websockets_mode import DataStreamingServer, _VideoRelay

res = H.Results("ws-gate-reprobe")
FPS = 60
TICK = 0.05


class Lines(logging.Handler):
    """The gate's log lines, by kind."""

    def __init__(self) -> None:
        super().__init__(logging.DEBUG)
        self.lines = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append((time.monotonic(), record.getMessage()))

    def count(self, text: str, since: float = 0.0) -> int:
        return sum(1 for t, m in self.lines if t >= since and text in m)


LOG = Lines()
logging.getLogger("ws").addHandler(LOG)
logging.getLogger("ws").setLevel(logging.DEBUG)


class Module:
    """The capture's encoder, as far as the gate reaches it."""

    def __init__(self) -> None:
        self.idrs = []

    def request_idr_frame(self) -> None:
        self.idrs.append(time.monotonic())


class Link:
    """The client's socket and the path behind it: frames and pings arrive in
    the order they were written, each after the bytes ahead of it crossed at
    `rate` bytes a second (instant when 0); a ping's pong comes back at once."""

    def __init__(self, rate: float = 0.0) -> None:
        self.rate = rate
        self.free_at = 0.0
        self.closed = False

    def carry(self, size: int) -> float:
        """When a write of `size` bytes made now reaches the client."""
        now = time.monotonic()
        self.free_at = max(now, self.free_at) + (size / self.rate if self.rate else 0.0)
        return self.free_at

    async def ping(self, payload: bytes) -> None:
        delay = max(0.0, self.carry(0) - time.monotonic())
        asyncio.get_running_loop().call_later(delay, note_pong, self, payload)


def make_server(module: Module, link: Link) -> DataStreamingServer:
    server = DataStreamingServer.__new__(DataStreamingServer)
    server.client_settings_received = None
    server.backpressure_check_interval_s = TICK
    server.allowed_desync_ms = w.BACKPRESSURE_ALLOWED_DESYNC_MS
    server.capture_instances = {"primary": {"module": module}}
    server.cli_args = SimpleNamespace(congestion_control=(False,))
    server.rc_mode = SimpleNamespace(value="cbr")
    server.metrics = None
    server.video_relay_groups = {}
    server.display_clients = {"primary": {
        "ws": link, "framerate": FPS, "acknowledged_frame_id": -1, "acked_sent_at": None,
        "last_sent_frame_id": 0, "has_sent_any_frame": False, "sent_timestamps": OrderedDict(),
        "sent_bytes": 0, "rtt_samples": deque(maxlen=20), "smoothed_rtt": 0.0,
        "backpressure_enabled": True, "unacked_since": None, "stall_gated_at": None,
    }}
    return server


async def play(server: DataStreamingServer, module: Module, client, seconds: float,
               big_at: float = -1.0, big: int = 0, key_size: int = 0) -> dict:
    """Run the loop for `seconds` with the capture producing FPS frames a second.

    The fan-out sends a frame only while the gate is open and stamps it as the
    relay does; after every lift it sends from the key frame the lift asked
    for, of `key_size` bytes where given. The link delivers each frame after
    the ones ahead of it; the first frame sent `big_at` seconds in is a key
    frame of `big` bytes. `client` is the page: it gets each frame as it
    arrives and says what to ack each tick. Sends and acks go through the
    server's own bookkeeping (`_note_send`, `_note_ack`).
    """
    ds = server.display_clients["primary"]
    link = ds["ws"]
    in_flight = deque()
    task = asyncio.ensure_future(server._run_frame_backpressure_logic("primary"))
    stats = {"produced": 0, "sent": 0, "closed_spans": [], "gated_since": None, "idrs_sent": 0}
    fid = 0
    last_ack = 0.0
    idrs_seen = 0
    start = time.monotonic()
    next_frame = start
    try:
        while time.monotonic() - start < seconds:
            now = time.monotonic()
            if now >= next_frame:
                next_frame += 1.0 / FPS
                fid = (fid + 1) & 0xFFFF
                stats["produced"] += 1
                open_ = ds.get("backpressure_enabled", True)
                if not open_ and stats["gated_since"] is None:
                    stats["gated_since"] = now
                if open_ and stats["gated_since"] is not None:
                    stats["closed_spans"].append(now - stats["gated_since"])
                    stats["gated_since"] = None
                if open_:
                    size = 2000
                    key = len(module.idrs) > idrs_seen
                    idrs_seen = len(module.idrs)
                    if big and big_at >= 0 and now - start >= big_at:
                        size, key, big = big, True, 0
                    elif key and key_size:
                        size = key_size
                    w._note_send(ds, fid, size)
                    stats["idrs_sent"] += int(key)
                    in_flight.append((link.carry(size), fid, key))
                    stats["sent"] += 1
            while in_flight and in_flight[0][0] <= now:
                _t, rfid, rkey = in_flight.popleft()
                client.frame(now, (rfid, rkey))
            if now - last_ack >= 0.05:
                last_ack = now
                aid = client.ack(now)
                if aid is not None:
                    w._note_ack(ds, aid, 0.0)
            await asyncio.sleep(0.004)
    finally:
        if stats["gated_since"] is not None:
            stats["closed_spans"].append(time.monotonic() - stats["gated_since"])
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
    return stats


class Client:
    """A page acking the newest frame it can use.

    `mode` from `freeze_at` on: `frozen` holds the ack at the last usable id and
    repeats it every second; `keygate` does the same until a key frame arrives
    (a decoder that lost its reference), then acks as before; `silent` sends
    nothing at all; `slow` acks what it received `lag` seconds ago (a queue the
    floor learned before it does not forgive); `live` acks the newest frame to
    arrive, as a full-frame client does.
    """

    def __init__(self, mode: str, freeze_at: float, lag: float = 0.0) -> None:
        self.mode = mode
        self.freeze_at = time.monotonic() + freeze_at
        self.lag = lag
        self.usable = None
        self.frames = deque()
        self.recovered_at = None
        self.last_sent = (None, 0.0)

    def frame(self, now: float, received) -> None:
        if received is None:
            return
        fid, key = received
        frozen = now >= self.freeze_at
        if self.mode == "slow":
            self.frames.append((now, fid))
        elif not frozen or self.mode not in ("frozen", "keygate", "silent"):
            self.usable = fid
        elif self.mode == "keygate" and self.recovered_at is None and key:
            self.recovered_at = now
            self.usable = fid
        elif self.mode == "keygate" and self.recovered_at is not None:
            self.usable = fid

    def ack(self, now: float):
        if self.mode == "silent" and now >= self.freeze_at:
            return None
        if self.mode == "slow":
            lag = self.lag if now >= self.freeze_at else 0.0
            while self.frames and self.frames[0][0] <= now - lag:
                self.usable = self.frames.popleft()[1]
        if self.usable is None:
            return None
        if self.usable != self.last_sent[0] or now - self.last_sent[1] >= 1.0:
            self.last_sent = (self.usable, now)
            return self.usable
        return None


def run(mode: str, seconds: float, freeze_at: float = 1.0, lag: float = 0.0, rate: float = 0.0,
        big_at: float = -1.0, big: int = 0, key_size: int = 0):
    module = Module()
    server = make_server(module, Link(rate))
    client = Client(mode, freeze_at, lag)
    mark = time.monotonic()
    stats = asyncio.run(play(server, module, client, seconds, big_at, big, key_size))
    return stats, client, module, mark


# The issue's reproducer: the ack freezes at one id while the screen keeps changing.
stats, client, module, mark = run("frozen", 8.0)
triggered = LOG.count("Backpressure TRIGGERED", mark)
reprobes = LOG.count("Re-probing desynced client", mark)
res.check("a frozen ack closes the gate", triggered >= 1, f"{triggered} triggers")
res.check("the gate reopens on an IDR however long the ack stays frozen", reprobes >= 2 and len(module.idrs) >= reprobes,
          f"{reprobes} re-probes, {len(module.idrs)} IDR requests in 7 s frozen")
longest = max(stats["closed_spans"], default=0.0)
res.check("no closed spell outlasts the re-probe by more than a check or two",
          longest <= w.STALLED_CLIENT_REPROBE_SECONDS + 4 * TICK + 0.3,
          f"longest closed {longest:.2f} s (re-probe after {w.STALLED_CLIENT_REPROBE_SECONDS} s)")
frozen_share = stats["sent"] / max(1, stats["produced"])
res.check("a client that stays frozen costs a key frame per re-probe and at most half the stream",
          frozen_share < 0.5 and stats["idrs_sent"] <= reprobes + 2,
          f"{stats['sent']}/{stats['produced']} frames ({frozen_share:.0%}), {stats['idrs_sent']} key frames")

# A decoder that lost its reference: it can ack nothing until a key frame arrives.
stats, client, module, mark = run("keygate", 8.0)
res.check("a client waiting for a key frame gets one from the first re-probe",
          client.recovered_at is not None and client.recovered_at - (mark + 1.0) < w.STALLED_CLIENT_REPROBE_SECONDS + 1.5,
          f"recovered {client.recovered_at and round(client.recovered_at - mark - 1.0, 2)} s after the freeze")
after = LOG.count("Backpressure TRIGGERED", (client.recovered_at or 0.0) + 0.3)
res.check("and, answering it, keeps its stream",
          client.recovered_at is not None and after == 0 and LOG.count("Re-probing desynced client", mark) == 1,
          f"{after} triggers after recovering")

# A slow client still catching up moves its ack: that is the gate doing its job.
stats, client, module, mark = run("slow", 7.0, freeze_at=2.0, lag=0.6)
res.check("a slow client whose acks keep moving is gated but never re-probed",
          LOG.count("Backpressure TRIGGERED", mark) >= 1 and LOG.count("Re-probing desynced client", mark) == 0,
          f"{LOG.count('Backpressure TRIGGERED', mark)} triggers, "
          f"{LOG.count('Re-probing desynced client', mark)} re-probes")

# A 700 KB key frame over a 2 Mbit/s path takes 2.8 s to arrive, and nothing behind it
# arrives sooner: the acks stand still as they do for a stuck client, but it is coming.
stats, client, module, mark = run("live", 8.0, rate=250_000, big_at=1.0, big=700_000)
res.check("a key frame still crossing a slow path closes the gate", LOG.count("Backpressure TRIGGERED", mark) >= 1,
          f"{LOG.count('Backpressure TRIGGERED', mark)} triggers")
res.check("and is waited for, not answered with another key frame",
          LOG.count("Re-probing desynced client", mark) == 0 and LOG.count("Backpressure LIFTED", mark) >= 1,
          f"{LOG.count('Re-probing desynced client', mark)} re-probes, "
          f"{LOG.count('Backpressure LIFTED', mark)} lifts")

# Over the same path every lift's key frame is 400 KB and takes 1.6 s to cross, while the
# stream behind it fits the path: once the frames queued behind the first lift's key frame
# are let arrive, the gate has nothing more to close on.
stats, client, module, mark = run("live", 10.0, rate=250_000, big_at=1.0, big=700_000, key_size=400_000)
triggers = LOG.count("Backpressure TRIGGERED", mark)
res.check("a lift's own key frame crossing a slow path does not close the gate again",
          triggers <= 1 and LOG.count("Backpressure LIFTED", mark) >= 1,
          f"{triggers} triggers, {LOG.count('Backpressure LIFTED', mark)} lifts, "
          f"{stats['idrs_sent']} key frames in 10 s")

# A client that sends nothing at all is the stall branch's.
stats, client, module, mark = run("silent", 9.0)
res.check("a silent client is not re-probed as a desynced one",
          LOG.count("Re-probing desynced client", mark) == 0, "")
res.check("the stall branch still re-probes it", LOG.count("Re-probing stalled client", mark) >= 1,
          f"{LOG.count('Client stall for', mark)} stalls, {LOG.count('Re-probing stalled client', mark)} re-probes")



def chunk(fid: int, key: bool) -> dict:
    """A full-frame video chunk as pixelflux wraps it, the header's row at 0."""
    head = bytes([0x04, 0x01 if key else 0x00, fid >> 8, fid & 0xFF]) + bytes(8)
    data = head + bytes(2000)
    return {"data": memoryview(data), "owner": data, "frame_id": fid}


# The gate closed, so every row of the client's relay waits for its key frame; the lift
# asks the encoder for one, and the deltas encoded before it still reach the relay first.
# The relay is built on a running loop, as the server builds it; Python 3.9's asyncio
# primitives bind a loop when they are made, and asyncio.run() above left none current.
async def relay_checks() -> None:
    module = Module()
    server = make_server(module, Link())
    ds = server.display_clients["primary"]
    relay = _VideoRelay(server, "primary", ds["ws"], 8 * 1024 * 1024)
    server.video_relay_groups["primary"] = {ds["ws"]: relay}
    relay.flush_for_gate()
    ds["backpressure_enabled"] = False
    server._set_backpressure_enabled("primary", ds, True)
    asks = [relay.offer(chunk(fid, False)) for fid in range(10, 13)]
    res.check("a lift asks for one key frame, and the deltas ahead of it ask for none",
              len(module.idrs) == 1 and not any(asks), f"{len(module.idrs)} from the lift, relay asks {asks}")
    relay.offer(chunk(13, True))
    res.check("and the stream resumes on it", relay.offer(chunk(14, False)) is False and len(relay.backlog) == 2,
              f"{len(relay.backlog)} chunks queued")
    relay.flush_for_gate()
    time.sleep(w.VIDEO_RELAY_SYNC_FLOOR_SECONDS + 0.1)
    res.check("a relay whose key frame never came asks again after the sync floor",
              relay.offer(chunk(15, False)) is True, "")


asyncio.run(relay_checks())

sys.exit(0 if res.summary() else 1)
