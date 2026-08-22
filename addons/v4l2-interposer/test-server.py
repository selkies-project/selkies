#!/usr/bin/env python3
"""Standalone test backend for the Selkies V4L2 interposer.

Serves the webcam socket and streams synthetic MJPEG frames, mimicking the
Selkies Python backend without any of its dependencies. Use it to validate the
interposer with a real consumer:

    python3 test-server.py &
    WEBCAM_LOG=1 LD_PRELOAD="$PWD/selkies_v4l2_interposer.so" \\
        ffmpeg -f v4l2 -i /dev/video0 -frames:v 10 out.mjpeg

Requires Pillow for JPEG synthesis (falls back to a canned JPEG otherwise).
"""

import argparse
import asyncio
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "src"))
from selkies.webcam import WebcamServer  # noqa: E402


def _make_frame(width: int, height: int, phase: int) -> bytes:
    try:
        from PIL import Image
        import io
        img = Image.new("RGB", (width, height),
                        (phase % 256, (phase * 3) % 256, (phase * 7) % 256))
        buf = io.BytesIO()
        img.save(buf, format="JPEG", quality=80)
        return buf.getvalue()
    except Exception:
        # Minimal valid 1x1 JPEG so the pipeline still exercises end to end.
        return bytes.fromhex(
            "ffd8ffe000104a46494600010100000100010000ffdb004300080606070605080707"
            "07090908 0a0c140d0c0b0b0c1912130f141d1a1f1e1d1a1c1c20242e2720222c231c"
            "1c2837292c30313434341f27393d38323c2e333432ffc0000b080001000101011100"
            "ffc4001f0000010501010101010100000000000000000102030405060708090a0bff"
            "c400b5100002010303020403050504040000017d01020300041105122131410613516"
            "107227114328191a1082342b1c11552d1f02433627282090a161718191a25262728"
            "292a3435363738393a434445464748494a535455565758595a636465666768696a73"
            "7475767778797a838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2"
            "b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e"
            "9eaf1f2f3f4f5f6f7f8f9faffda0008010100003f00fbfeffd9".replace(" ", "")
        )


async def _run(args: argparse.Namespace) -> None:
    server = WebcamServer(args.socket, args.width, args.height, args.fps, 1)
    await server.start()
    print(f"Serving {args.socket}; run a consumer under the interposer.")
    phase = 0
    interval = 1.0 / max(1, args.fps)
    try:
        while True:
            if server.has_clients():
                server.feed(_make_frame(args.width, args.height, phase))
                phase += 1
            await asyncio.sleep(interval)
    finally:
        await server.stop()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", default="/tmp/selkies_webcam0.sock")
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=720)
    parser.add_argument("--fps", type=int, default=30)
    args = parser.parse_args()
    try:
        asyncio.run(_run(args))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
