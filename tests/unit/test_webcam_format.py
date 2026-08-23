#!/usr/bin/env python3
"""``webcam_pixel_format`` resolution: ``auto`` follows the codec of the frame that brings the
camera up (an MJPEG uplink becomes an MJPEG device, anything else I420); an explicit format is
used as given."""
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "src"))

failures = 0


def check(name: str, ok: bool, detail: str = "") -> None:
    global failures
    print(("PASS  " if ok else "FAIL  ") + name + (f"  {detail}" if detail and not ok else ""))
    if not ok:
        failures += 1


def main() -> int:
    from selkies import webcam as wc

    check("auto + MJPEG uplink -> MJPEG device", wc.device_pixel_format("auto", wc.CODEC_MJPEG) == "MJPEG")
    check("auto + H.264 uplink -> I420 device", wc.device_pixel_format("auto", wc.CODEC_H264) == "I420")
    check("auto + VP8 uplink -> I420 device", wc.device_pixel_format("auto", wc.CODEC_VP8) == "I420")
    check("auto without a codec -> I420 device", wc.device_pixel_format("auto", None) == "I420")
    check("explicit I420 stays I420 for an MJPEG uplink", wc.device_pixel_format("I420", wc.CODEC_MJPEG) == "I420")
    check("explicit MJPEG stays MJPEG for an H.264 uplink", wc.device_pixel_format("MJPEG", wc.CODEC_H264) == "MJPEG")
    check("explicit formats keep their spelling, trimmed", wc.device_pixel_format(" NV12 ", None) == "NV12")
    check("an empty setting behaves as auto", wc.device_pixel_format("", wc.CODEC_MJPEG) == "MJPEG")
    print(f"\n{failures} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
