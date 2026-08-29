#!/usr/bin/env python3
"""Audio while a large clipboard crosses the session socket.

Audio, video, input and the clipboard share one ordered connection, so a
transfer written as fast as the socket accepts it leaves megabytes in front of
the next audio packet -- and the socket buffer absorbs them without the
application ever seeing backpressure, so on a real link the stream stops for
as long as the buffer takes to drain. The measurement needs a link that is not
loopback: a metered relay stands in for one, and the check is the worst gap
between audio frames while the transfer runs.

A raw client, so what is measured is the connection's own delivery rather than
a browser's jitter buffer smoothing it over.

Usage: python3 tests/e2e/test_clipboard_pacing.py [websockets|wayland]
"""
import asyncio
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

try:
    import websockets
except ImportError:
    H.skip_suite("the websockets package is not installed")

# The link the relay meters, in bytes per second. Slow enough that a transfer
# left unpaced is unmistakable, fast enough to carry the stream itself.
LINK_BYTES_PER_S = 1_250_000
RELAY_PORT = int(os.environ.get("E2E_PACING_PORT", "18197"))
CLIPBOARD_MB = 8
# What a jitter buffer of a few 10 ms packets absorbs. An unpaced transfer
# overruns this by two orders of magnitude, so the bound needs no margin.
MAX_GAP_S = 0.5

SETTINGS = {
    "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
    "manual_resolution": False, "encoder": "jpeg", "framerate": 30,
    "video_crf": 25, "video_bitrate": 4000, "audio_bitrate": 128000,
    "scaling_dpi": 96, "displayPosition": "right",
}

RELAY = r"""
import asyncio, sys, time

async def pump(reader, writer, rate, mtu=1500):
    allowance, last = 0.0, time.monotonic()
    try:
        while True:
            data = await reader.read(mtu)
            if not data:
                break
            now = time.monotonic()
            allowance = min(allowance + (now - last) * rate, rate * 0.05)
            last = now
            allowance -= len(data)
            if allowance < 0:
                await asyncio.sleep(-allowance / rate)
                allowance, last = 0.0, time.monotonic()
            writer.write(data)
            await writer.drain()
    except Exception:
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass

async def main(listen, target, rate):
    async def handle(cr, cw):
        try:
            sr, sw = await asyncio.open_connection("127.0.0.1", target)
        except OSError:
            cw.close()
            return
        await asyncio.gather(pump(cr, sw, rate), pump(sr, cw, rate))
    server = await asyncio.start_server(handle, "127.0.0.1", listen)
    async with server:
        await server.serve_forever()

asyncio.run(main(int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])))
"""


def load_sine() -> str:
    """A tone into the capture sink, so the session has audio to stream."""
    r = subprocess.run(["pactl", "load-module", "module-sine", "sink=output",
                        "frequency=440"], capture_output=True, text=True)
    return r.stdout.strip()


def push_clipboard(wayland: bool, payload: bytes) -> dict:
    """Put `payload` on the session clipboard as text, as an application would."""
    if wayland:
        env = {**os.environ, "WAYLAND_DISPLAY": "wayland-1",
               "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", H.WORKDIR)}
        proc = subprocess.Popen(["wl-copy", "-t", "text/plain"],
                                stdin=subprocess.PIPE, env=env)
        proc.stdin.write(payload)
        proc.stdin.close()
        return {"stop": lambda: None}
    _display, state = H.x_own_clipboard(payload)
    return {"stop": lambda: state.__setitem__("flag", True)}


async def measure(port: int, wayland: bool) -> tuple:
    """Audio frame gaps before and during a clipboard push.

    Returns:
        `(idle_gaps, push_gaps, frames)` in seconds.
    """
    uri = f"ws://localhost:{port}/api/websockets"
    stamps: list = []
    marks: dict = {}
    holder: dict = {}
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=15)
        await ws.send("SETTINGS," + json.dumps(SETTINGS))
        await ws.send("START_AUDIO")

        async def pump():
            while True:
                message = await ws.recv()
                if isinstance(message, (bytes, bytearray)) and message and message[0] == 0x01:
                    stamps.append(time.monotonic())

        reader = asyncio.create_task(pump())
        try:
            await asyncio.sleep(8.0)
            marks["idle"] = time.monotonic()
            await asyncio.sleep(10.0)
            marks["push"] = time.monotonic()
            block = ("CLIP" + "x" * 4092).encode() * (CLIPBOARD_MB * 256)
            holder = push_clipboard(wayland, block)
            await asyncio.sleep(25.0)
            marks["done"] = time.monotonic()
        finally:
            reader.cancel()
            if holder:
                holder["stop"]()

    def gaps(start, end):
        window = [t for t in stamps if start <= t <= end]
        return [b - a for a, b in zip(window, window[1:])]

    return (gaps(marks["idle"], marks["push"]),
            gaps(marks["push"], marks["done"]),
            len(stamps))


def block(wayland: bool) -> "H.Results":
    tag = f"clippacing-{'wl' if wayland else 'x11'}"
    res = H.Results(tag)
    module = load_sine()
    relay = None
    try:
        H.server_start(mode="websockets", wayland=wayland)
        relay = H.spawn([sys.executable, "-c", RELAY, str(RELAY_PORT),
                         str(H.PORT), str(LINK_BYTES_PER_S)])
        time.sleep(1.0)
        idle, during, frames = asyncio.run(measure(RELAY_PORT, wayland))
        res.check("audio streams over the metered link", frames > 100 and idle,
                  f"{frames} frames")
        if not idle or not during:
            return res
        res.check("audio is even before the transfer", max(idle) < MAX_GAP_S,
                  f"worst idle gap {max(idle) * 1000:.0f} ms")
        res.check("a large clipboard does not stall the audio",
                  max(during) < MAX_GAP_S,
                  f"worst gap {max(during) * 1000:.0f} ms during "
                  f"{CLIPBOARD_MB} MB (idle {max(idle) * 1000:.0f} ms)")
    finally:
        if relay is not None:
            relay.kill()
        H.server_stop()
        if module:
            subprocess.run(["pactl", "unload-module", module], capture_output=True)
    res.summary()
    return res


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "websockets"
    result = block(which == "wayland")
    print(f"\n=== CLIPBOARD PACING: {'FAIL' if result.failed() else 'PASS'} ===")
    sys.exit(1 if result.failed() else 0)


if __name__ == "__main__":
    main()
