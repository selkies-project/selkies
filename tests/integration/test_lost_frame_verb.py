#!/usr/bin/env python3
"""A client that drops a frame tells the server, and the encoder predicts past it.

A decoder that cannot keep up lets a frame go, which breaks every frame
predicting from it. Rather than freeze until a key frame arrives, the client
names the frame it dropped (`LOST_FRAME <id>`) and the server has that
display's encoder leave it out of every later prediction, so the next frame
decodes there on its own. The verb is floored per display, since any number of
clients share one encoder, and a malformed one is ignored rather than taken as
a frame id.

Driven with a raw websockets client: what is under test is the server's answer
to the verb, not a browser's decision to send it.

Usage: python3 tests/integration/test_lost_frame_verb.py
"""
import asyncio
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets

TOLD = "the encoder predicts past it"
#: The wire id VP9 frames carry in the high nibble of their type byte.
VP9 = 3


def settings(dpi: int = 96, encoder: str = "h264enc") -> str:
    return "SETTINGS," + json.dumps({
        "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": False, "framerate": 60, "encoder": encoder,
        "video_crf": 25, "video_bitrate": 8000, "audio_bitrate": 128000,
        "scaling_dpi": dpi,
    })


async def saw(mark: int, substr: str, timeout: float = 15) -> bool:
    """Whether the server log shows `substr` at or after `mark` within the timeout."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if H.server_log().find(substr, mark) >= 0:
            return True
        await asyncio.sleep(0.2)
    return False


async def drain(ws, out: list = None) -> None:
    """Read the stream, noting each video frame's codec, id and whether it decodes alone."""
    while True:
        try:
            msg = await ws.recv()
        except Exception:
            return
        if out is not None and isinstance(msg, (bytes, bytearray)) and len(msg) > 12 and msg[0] == 0x04:
            out.append((msg[1] >> 4, int.from_bytes(msg[2:4], "big"), msg[1] & 0x0f == 0x01))


async def key_frame_repair(res: "H.Results") -> None:
    """A session whose encoder cannot leave the frame out is repaired with a key frame.

    Only libx264 and NVENC predict past a lost frame; every codec libavcodec
    drives names no reference, and pixelflux codes a key frame there instead.
    No backend tracks a VP9 session, so this arm reads the same on any host.
    """
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send(settings(encoder="vp9enc"))
        seen: list = []
        pump = asyncio.create_task(drain(ws, seen))
        # The switch restarts the capture, so wait for the codec that was asked
        # for rather than measuring the stream it replaces.
        deadline = time.time() + 25
        while time.time() < deadline and not any(c == VP9 for c, _, _ in seen):
            await asyncio.sleep(0.5)
        res.check("the VP9 session streams", any(c == VP9 for c, _, _ in seen),
                  f"{len(seen)} frames, codecs {sorted({c for c, _, _ in seen})}")
        vp9 = [f for f in seen if f[0] == VP9]
        if vp9:
            last = vp9[-1][1]
            seen.clear()
            await ws.send(f"LOST_FRAME {last}")
            await asyncio.sleep(2.0)
            frames = [f for f in seen if f[0] == VP9]
            keys = sum(1 for _, _, key in frames if key)
            res.check("a session that cannot predict past it is repaired with a key frame",
                      keys > 0, f"{len(frames)} frames, {keys} key")
        pump.cancel()


async def drive() -> "H.Results":
    res = H.Results("lost-frame-verb")
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send(settings())
        pump = asyncio.create_task(drain(ws))
        mark = len(H.server_log())
        await ws.send("LOST_FRAME 41")
        res.check("the encoder is told to predict past the frame a client lost",
                  await saw(mark, TOLD), H.server_log()[mark:][-200:])
        res.check("and the line names the frame", "frame 41 lost by a client" in H.server_log()[mark:],
                  H.server_log()[mark:][-200:])

        mark = len(H.server_log())
        for frame_id in range(100, 140):
            await ws.send(f"LOST_FRAME {frame_id}")
        await asyncio.sleep(1.0)
        told = H.server_log()[mark:].count(TOLD)
        res.check("a burst of them adds no flood of log lines",
                  told <= 1, told)

        mark = len(H.server_log())
        await ws.send("LOST_FRAME nonsense")
        await ws.send("LOST_FRAME")
        await asyncio.sleep(0.5)
        res.check("a malformed report is ignored rather than taken as a frame id",
                  TOLD not in H.server_log()[mark:], H.server_log()[mark:][-200:])

        mark = len(H.server_log())
        await ws.send("REQUEST_KEYFRAME")
        res.check("the session still answers, so nothing about the verb wedged it",
                  await saw(mark, "Keyframe requested by"), H.server_log()[mark:][-200:])
        pump.cancel()
    await asyncio.sleep(1.0)
    await key_frame_repair(res)
    res.summary()
    return res


def main() -> None:
    H.server_start(mode="websockets", wayland=False)
    r = asyncio.run(drive())
    sys.exit(0 if not r.failed() else 1)


if __name__ == "__main__":
    main()
