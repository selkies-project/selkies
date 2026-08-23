#!/usr/bin/env python3
"""Standalone backend for exercising the Selkies V4L2 interposer.

Starts a ``pixelflux.VirtualCamera`` on the interposer socket and feeds it
synthetic MJPEG frames (a moving colour field), standing in for a browser's
camera uplink. Run it, then run any consumer under the interposer::

    python3 test-server.py &
    WEBCAM_LOG=1 LD_PRELOAD="$PWD/selkies_v4l2_interposer.so" \\
        ffmpeg -f v4l2 -i /dev/video0 -frames:v 10 out.mkv

Requires pixelflux and Pillow.
"""

import argparse
import asyncio
import io
import sys

from PIL import Image

import pixelflux


def _make_frame(width: int, height: int, phase: int) -> bytes:
    img = Image.new("RGB", (width, height), (phase % 256, (phase * 3) % 256, (phase * 7) % 256))
    bar_w = max(1, width // 8)
    for i in range(8):
        shade = (i * 32 + phase) % 256
        img.paste((shade, 255 - shade, (shade * 5) % 256), (i * bar_w, 0, (i + 1) * bar_w, height // 4))
    buf = io.BytesIO()
    img.save(buf, format="JPEG", quality=80)
    return buf.getvalue()


async def _run(args: argparse.Namespace) -> None:
    settings = pixelflux.VirtualCameraSettings()
    settings.socket_path = args.socket
    settings.width = args.width
    settings.height = args.height
    settings.fps_num = args.fps
    settings.fps_den = 1
    settings.pixel_format = args.pixel_format
    settings.device_path = args.device
    cam = pixelflux.VirtualCamera()
    cam.start(settings)
    print(f"Serving {args.socket} ({args.width}x{args.height} {args.pixel_format} @ {args.fps} fps); "
          f"run a consumer under the interposer.", flush=True)
    phase = 0
    interval = 1.0 / max(1, args.fps)
    source_w = args.source_width or args.width
    source_h = args.source_height or args.height
    try:
        while True:
            if cam.clients or args.always:
                cam.push(_make_frame(source_w, source_h, phase), pixelflux.VirtualCamera.CODEC_MJPEG, True)
                phase += 1
                if args.stats and phase % args.fps == 0:
                    print(cam.stats(), flush=True)
            await asyncio.sleep(interval)
    finally:
        cam.stop()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--socket", default="/tmp/selkies_webcam0.sock")
    parser.add_argument("--width", type=int, default=1280, help="advertised device width")
    parser.add_argument("--height", type=int, default=720, help="advertised device height")
    parser.add_argument("--fps", type=int, default=30)
    parser.add_argument("--pixel-format", default="I420", help="I420, NV12 or YUYV")
    parser.add_argument("--device", default="", help='v4l2loopback output device to mirror ("auto" or a path)')
    parser.add_argument("--source-width", type=int, default=0, help="synthetic camera width (default: device width)")
    parser.add_argument("--source-height", type=int, default=0, help="synthetic camera height (default: device height)")
    parser.add_argument("--always", action="store_true", help="feed frames even with no consumer connected")
    parser.add_argument("--stats", action="store_true", help="print camera counters once per second")
    args = parser.parse_args()
    try:
        asyncio.run(_run(args))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    sys.exit(main())
