#!/usr/bin/env python3
"""The 0x06 flags byte carries the frame's upright transform next to the
keyframe bit: bits 1-2 the clockwise rotation in quarter turns, bit 3 a
horizontal flip applied after the rotation. The bit layout is wire ABI shared
with the web client's sendFrame, so it is pinned here, and the optional
``rotation``/``flip`` arguments of ``VirtualWebcam.push`` must degrade to the
old four-argument call against a pixelflux build that predates them (announced
once, never retried per frame).
"""
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "src"))

from selkies.webcam import (  # noqa: E402
    WS_FLAG_HFLIP,
    WS_FLAG_KEYFRAME,
    WS_FLAG_ROTATION_MASK,
    WS_FLAG_ROTATION_SHIFT,
    VirtualWebcam,
    orientation_from_flags,
)

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [webcam-orient] {label}  {detail}", flush=True)


class OldCam:
    """A pixelflux VirtualCamera whose push predates the orientation arguments."""

    def __init__(self):
        self.calls = []

    def push(self, data, codec, keyframe=False, offset=0):
        self.calls.append((codec, keyframe, offset))
        return 0


class NewCam(OldCam):
    """Records exactly the arguments given, so a short call stays a 3-tuple."""

    def push(self, data, *args, **kwargs):
        self.calls.append(args + tuple(kwargs.values()))
        return 0


def main() -> int:
    check("flag bits pinned",
          (WS_FLAG_KEYFRAME, WS_FLAG_ROTATION_SHIFT, WS_FLAG_ROTATION_MASK, WS_FLAG_HFLIP)
          == (0x01, 1, 0x06, 0x08))
    cases = {
        0x00: (0, False), 0x01: (0, False), 0x03: (90, False), 0x05: (180, False),
        0x07: (270, False), 0x09: (0, True), 0x0F: (270, True),
    }
    for flags, expected in cases.items():
        got = orientation_from_flags(flags)
        check(f"flags 0x{flags:02x} -> {expected}", got == expected, f"got {got}")

    wc = VirtualWebcam()
    check("no camera drops silently", wc.push(b"x", 1, True, 3, 180, False) == 0)

    wc._cam = new = NewCam()
    wc.push(b"x", 1, True, 3, 180, False)
    check("orientation forwarded", new.calls[-1] == (1, True, 3, 180, False), str(new.calls[-1]))
    wc.push(b"x", 1, False, 3, 0, True)
    check("flip alone forwarded", new.calls[-1] == (1, False, 3, 0, True), str(new.calls[-1]))
    wc.push(b"x", 1, False, 3)
    check("upright frames keep the short call", new.calls[-1] == (1, False, 3), str(new.calls[-1]))

    wc = VirtualWebcam()
    wc._cam = old = OldCam()
    wc.push(b"x", 2, True, 3, 90, False)
    check("old pixelflux falls back", old.calls == [(2, True, 3)], str(old.calls))
    check("fallback latched", wc._push_orientation is False)
    wc.push(b"x", 2, False, 3, 270, True)
    check("no per-frame retry", old.calls == [(2, True, 3), (2, False, 3)], str(old.calls))

    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
