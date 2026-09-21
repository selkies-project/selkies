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
#: The wire ids VP9 and H.265 frames carry in the high nibble of their type byte.
VP9 = 3
H265 = 5


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
    """Read the stream, noting each video frame's codec, id, whether it decodes alone, and the
    frame it predicts from (its own id where it names none)."""
    while True:
        try:
            msg = await ws.recv()
        except Exception:
            return
        if out is not None and isinstance(msg, (bytes, bytearray)) and len(msg) > 12 and msg[0] == 0x04:
            out.append((msg[1] >> 4, int.from_bytes(msg[2:4], "big"), msg[1] & 0x0f == 0x01,
                        int.from_bytes(msg[10:12], "big")))


async def stream(ws, codec: int, seen: list) -> list:
    """The frames of `codec` once the switch to it has taken: the switch restarts the capture,
    so the stream it replaces is not what is measured."""
    deadline = time.time() + 25
    while time.time() < deadline and not any(c == codec for c, *_ in seen):
        await asyncio.sleep(0.5)
    return [f for f in seen if f[0] == codec]


async def lost_frame_answers(res: "H.Results") -> None:
    """A lost frame is predicted past where the encoder can name its references, and repaired
    with a key frame where it cannot.

    Software VP9 tracks its references through libvpx's flexible reference mode, so the frame
    after the report predicts from one before the loss and no key frame is spent; a hardware
    VP9 session on a render node does the same through its reference slots. Software H.265
    (x265 or kvazaar) offers nothing to steer its references with, names none, and pixelflux
    codes a key frame there instead; on a host whose GPU takes the H.265 session the encoder
    predicts past the loss as VP9 does, which the server's encoder line tells apart.
    """
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        seen: list = []
        pump = asyncio.create_task(drain(ws, seen))
        for encoder, codec, name in [("vp9enc", VP9, "VP9"), ("h265enc", H265, "H.265")]:
            seen.clear()
            mark = len(H.server_log())
            await ws.send(settings(encoder=encoder))
            frames = await stream(ws, codec, seen)
            res.check(f"the {name} session streams", bool(frames),
                      f"{len(seen)} frames, codecs {sorted({c for c, *_ in seen})}")
            if not frames:
                continue
            tracked = codec == VP9 or "Encoder: software H265" not in H.server_log()[mark:]
            last = frames[-1][1]
            seen.clear()
            await ws.send(f"LOST_FRAME {last}")
            await asyncio.sleep(2.0)
            frames = [f for f in seen if f[0] == codec]
            keys = sum(1 for _, _, key, _ in frames if key)
            # A frame or two encoded before the report reached the encoder still
            # predict from the lost one; the ones after it reach back past it.
            behind = [f for f in frames if not f[2] and f[3] != last and (last - f[3]) % 65536 < 0x8000]
            detail = (f"{len(frames)} frames, {keys} key, {len(behind)} predicting from before the loss, "
                      f"first from {frames[0][3] if frames else None}, lost {last}")
            if tracked:
                res.check(f"{name}: the frames after the report predict from before the loss and cost no key frame",
                          keys == 0 and bool(behind), detail)
            else:
                res.check(f"{name}: a session that cannot predict past it is repaired with a key frame",
                          keys > 0, detail)
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
    await lost_frame_answers(res)
    res.summary()
    return res


def main() -> None:
    H.server_start(mode="websockets", wayland=False, extra_env={"SELKIES_DEBUG": "true"})
    r = asyncio.run(drive())
    sys.exit(0 if not r.failed() else 1)


if __name__ == "__main__":
    main()
