#!/usr/bin/env python3
"""How the server tells an idle client from a dead one, and how a gated client
gets its stream back, driven by a raw WebSocket client on a damage-gated
screen (turbo off).

A still screen sends no frames, so silence is not a stall: the server marks a
client stalled only when a frame it sent goes unanswered, and a client that
repeats its last ack as a heartbeat, or one that acks only new ids, is left
alone. After the still period the first frame must not read as a lagging
client either: ids run at the capture cadence, so their distance overstates
how far behind a client that was sent nothing is. A client that stops acking
under motion is gated and re-probed on a keyframe, one that stops reading gets
its stream back once it drains, and one that never acks again costs a
keyframe per re-probe rather than a stream. The gate is shared by both
backends and both transports' fan-out, so one X11 run covers it.

Uses `E2E_DISPLAY` when set; otherwise starts a throwaway Xvfb.
Usage: python3 tests/integration/test_stall_recovery.py [h264enc|jpeg|all]
"""
import asyncio
import json
import os
import sys
import time
from typing import Any, List, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
import websockets

SETTINGS = {"displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": False, "framerate": 60, "video_crf": 25,
            "video_bitrate": 6000, "audio_bitrate": 128000, "scaling_dpi": 96,
            "displayPosition": "right"}


def log_mark() -> int:
    return os.path.getsize(H.LOG) if os.path.exists(H.LOG) else 0


def events(mark: int) -> dict:
    """The gate's log lines since `mark`, counted by kind."""
    with open(H.LOG, "rb") as f:
        f.seek(mark)
        text = f.read().decode("utf-8", "replace")
    return {"stall": text.count("Client stall for"),
            "reprobe": text.count("Re-probing stalled client"),
            "lifted": text.count("Backpressure LIFTED"),
            "triggered": text.count("Backpressure TRIGGERED"),
            "traceback": text.count("Traceback")}


class Client:
    """A display's client on the raw protocol: frames in, acks out."""

    def __init__(self, ws: Any) -> None:
        self.ws = ws
        self.last_id = -1
        self.last_at = 0.0
        self.frames: List[Tuple[float, int, bool]] = []
        self.hb_at = 0.0

    async def pump(self, seconds: float, ack: str = "new", read: bool = True) -> None:
        """Receive for `seconds`.

        Args:
            seconds: How long to run.
            ack: `new` acks each new frame id, `heartbeat` also repeats the
                last id every second as the client does, `none` sends nothing.
            read: False leaves the socket unread, as a suspended app does.
        """
        end = time.monotonic() + seconds
        if not read:
            await asyncio.sleep(seconds)
            return
        while time.monotonic() < end:
            try:
                m = await asyncio.wait_for(self.ws.recv(), timeout=0.2)
            except asyncio.TimeoutError:
                m = None
            now = time.monotonic()
            if isinstance(m, (bytes, bytearray)) and len(m) >= 4 and m[0] in (0x03, 0x04):
                fid = (m[2] << 8) | m[3]
                idr = m[0] == 0x04 and m[1] == 0x01
                if fid != self.last_id:
                    self.last_id, self.last_at = fid, now
                    self.frames.append((now, fid, idr))
                    if ack != "none":
                        await self.ws.send(f"CLIENT_FRAME_ACK {fid} 0")
                        self.hb_at = now
            if ack == "heartbeat" and self.last_id >= 0 and now - self.hb_at >= 1.0:
                held = int((now - self.last_at) * 1000)
                await self.ws.send(f"CLIENT_FRAME_ACK {self.last_id} {held}")
                self.hb_at = now

    def since(self, t0: float) -> List[Tuple[float, int, bool]]:
        return [f for f in self.frames if f[0] >= t0]


async def connect(encoder: str) -> Client:
    ws = await websockets.connect(f"ws://localhost:{H.PORT}/api/websockets", max_size=None)
    await asyncio.wait_for(ws.recv(), timeout=10)
    await ws.send("SETTINGS," + json.dumps(dict(SETTINGS, encoder=encoder)))
    return Client(ws)


async def still_screen(res: H.Results, tag: str, encoder: str, kind: str) -> None:
    """Motion, a still screen for 10 s, then motion again; `kind` is what the
    client sends meanwhile: the heartbeat, or new ids only (an older client)."""
    ack = "heartbeat" if kind == "heartbeat" else "new"
    churn = C.Churn()
    churn.start()
    c = await connect(encoder)
    try:
        await c.pump(4, "new")
        res.check(f"{tag} {kind}: frames flow with motion", len(c.frames) > 20, f"{len(c.frames)} frames")
        mark = log_mark()
        churn.stop()
        t0 = time.monotonic()
        await c.pump(10, ack)
        quiet = c.since(t0 + 1.5)
        ev = events(mark)
        res.check(f"{tag} {kind}: a still screen sends nothing", len(quiet) == 0,
                  f"{len(quiet)} frames after the repaint")
        res.check(f"{tag} {kind}: a still screen is not a stall", ev["stall"] == 0, ev)
        mark = log_mark()
        t1 = time.monotonic()
        churn.start()
        await c.pump(4, ack)
        fresh = c.since(t1)
        first = fresh[0][0] - t1 if fresh else None
        gaps = [b[0] - a[0] for a, b in zip(fresh, fresh[1:]) if b[0] - t1 < 2.5]
        ev = events(mark)
        res.check(f"{tag} {kind}: motion resumes the stream at once",
                  first is not None and first < 1.5, f"first frame after {first and round(first, 2)} s")
        res.check(f"{tag} {kind}: the resume is not gated as a lagging client",
                  ev["triggered"] == 0 and ev["lifted"] == 0, ev)
        res.check(f"{tag} {kind}: no freeze follows the first frame",
                  bool(gaps) and max(gaps) < 0.3, f"max gap {gaps and round(max(gaps), 2)} s over {len(gaps)} frames")
    finally:
        await c.ws.close()
        churn.stop()


async def silent(res: H.Results, tag: str, encoder: str) -> None:
    """The client keeps reading but stops acking for 5 s under motion."""
    churn = C.Churn()
    churn.start()
    c = await connect(encoder)
    try:
        await c.pump(3, "new")
        mark = log_mark()
        await c.pump(5, "none")
        ev = events(mark)
        res.check(f"{tag} silent: a client that stops acking is gated", ev["stall"] + ev["triggered"] >= 1, ev)
        res.check(f"{tag} silent: the re-probe waits its turn", ev["reprobe"] == 0, ev)
        mark = log_mark()
        t1 = time.monotonic()
        await c.ws.send(f"CLIENT_FRAME_ACK {c.last_id} 0")
        await c.pump(5, "heartbeat")
        fresh = c.since(t1)
        first = fresh[0][0] - t1 if fresh else None
        ev = events(mark)
        res.check(f"{tag} silent: the stream resumes once the client acks again",
                  first is not None and first < 1.5 and len(fresh) > 20,
                  f"first frame after {first and round(first, 2)} s, {len(fresh)} frames, {ev}")
        res.check(f"{tag} silent: the lift opens on a keyframe",
                  ev["lifted"] >= 1 and bool(fresh) and (encoder != "h264enc" or fresh[0][2]), f"{fresh[:1]} {ev}")
    finally:
        await c.ws.close()
        churn.stop()


async def suspended(res: H.Results, tag: str, encoder: str) -> None:
    """The client stops reading and acking for 8 s, as a suspended app does.

    Two ends are right. Where the socket buffers hold the pause, the client
    is gated, re-probed and gets live frames once it drains. Where they do
    not, a send blocks past the liveness bound and the server drops the
    socket on purpose, which the page answers with a reconnect.
    """
    churn = C.Churn()
    churn.start()
    c = await connect(encoder)
    try:
        await c.pump(3, "new")
        mark = log_mark()
        await c.pump(8, "none", read=False)
        t1 = time.monotonic()
        try:
            await c.pump(8, "heartbeat")
        except websockets.ConnectionClosed as e:
            with open(H.LOG, "rb") as f:
                f.seek(mark)
                dropped = "send stalled past" in f.read().decode("utf-8", "replace")
            res.check(f"{tag} suspended: a socket the buffers could not hold was dropped on purpose",
                      dropped, f"{str(e)[:80]}; server log {'names' if dropped else 'lacks'} the stalled send")
            return
        ev = events(mark)
        late = c.since(t1 + 4)
        res.check(f"{tag} suspended: the client was gated and re-probed",
                  ev["stall"] + ev["triggered"] >= 1 and ev["reprobe"] >= 1, ev)
        res.check(f"{tag} suspended: live frames flow again once the backlog drains",
                  len(late) > 20, f"{len(late)} frames in the last 4 s, {ev}")
    finally:
        await c.ws.close()
        churn.stop()


async def dead(res: H.Results, tag: str, encoder: str) -> None:
    """The client reads but never acks again."""
    churn = C.Churn()
    churn.start()
    c = await connect(encoder)
    try:
        await c.pump(3, "new")
        mark = log_mark()
        t0 = time.monotonic()
        await c.pump(20, "none")
        ev = events(mark)
        idrs = [f for f in c.since(t0 + 5) if f[2]]
        res.check(f"{tag} dead: the gate cycles gate, re-probe, gate",
                  ev["stall"] + ev["triggered"] >= 2 and ev["reprobe"] >= 2, ev)
        if encoder == "h264enc":
            res.check(f"{tag} dead: each re-probe costs one keyframe, not a stream",
                      1 <= len(idrs) <= ev["reprobe"] + 1, f"{len(idrs)} IDRs for {ev['reprobe']} re-probes")
        res.check(f"{tag} dead: nothing raised in the server", ev["traceback"] == 0, ev)
    finally:
        await c.ws.close()
        churn.stop()


async def drive(res: H.Results, encoder: str) -> None:
    tag = f"[{encoder}]"
    await still_screen(res, tag, encoder, "heartbeat")
    await still_screen(res, tag, encoder, "new-ids-only")
    await silent(res, tag, encoder)
    await suspended(res, tag, encoder)
    await dead(res, tag, encoder)


def main(selection: str) -> H.Results:
    res = H.Results("stall-recovery")
    encoders = ("h264enc", "jpeg") if selection == "all" else (selection,)
    xproc = None
    if not H.TEST_DISPLAY:
        xproc, H.TEST_DISPLAY = H.private_x_server()
    try:
        for encoder in encoders:
            H.server_start(mode="websockets", wayland=False, extra_env={
                "SELKIES_USE_CPU": "true", "SELKIES_VIDEO_STREAMING_MODE": "false",
                "SELKIES_ENCODER": encoder})
            asyncio.run(drive(res, encoder))
    finally:
        H.server_stop()
        if xproc is not None:
            H.stop_x_server(xproc, H.TEST_DISPLAY)
    res.summary()
    return res


if __name__ == "__main__":
    r = main(sys.argv[1] if len(sys.argv) > 1 else "all")
    sys.exit(0 if not r.failed() else 1)
