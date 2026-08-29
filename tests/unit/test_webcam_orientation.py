#!/usr/bin/env python3
"""The 0x06 flags byte carries the frame's upright transform next to the
keyframe bit: bits 1-2 the clockwise rotation in quarter turns, bit 3 a
horizontal flip applied after the rotation. The bit layout is wire ABI shared
with the web client's sendFrame, so it is pinned here, along with the
``rotation``/``flip`` arguments ``VirtualWebcam.push`` hands the camera.

The client half -- which transform it puts on a frame, on which engines it
derives one, and what it does with a frame the socket cannot take -- is
JavaScript, so it is checked by ``tests/tools/webcam_orientation_audit.mjs``,
run from here so both sides of the same wire field fail together.
"""
import os
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "src"))
AUDIT = os.path.join(ROOT, "tests", "tools", "webcam_orientation_audit.mjs")

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


class Cam:
    """A stand-in pixelflux VirtualCamera recording exactly the arguments given."""

    def __init__(self):
        self.calls = []

    def push(self, data, *args, **kwargs):
        self.calls.append(args + tuple(kwargs.values()))
        return 0


def main() -> int:
    global passed, failed
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

    wc._cam = cam = Cam()
    wc.push(b"x", 1, True, 3, 180, False)
    check("orientation forwarded", cam.calls[-1] == (1, True, 3, 180, False), str(cam.calls[-1]))
    wc.push(b"x", 1, False, 3, 0, True)
    check("flip alone forwarded", cam.calls[-1] == (1, False, 3, 0, True), str(cam.calls[-1]))
    wc.push(b"x", 1, False, 3)
    check("upright frames carry a zero orientation",
          cam.calls[-1] == (1, False, 3, 0, False), str(cam.calls[-1]))

    node = shutil.which("node")
    if not node:
        # A skip rather than a pass: the client half is the other end of the same
        # wire field, and exiting zero here would announce it as checked.
        print("SKIP node not found, so the client orientation audit cannot run", flush=True)
        print(f"\n{passed} passed, {failed} failed")
        return 77
    audit = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
    lines = [ln for ln in audit.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
    for line in lines:
        print(line, flush=True)
    if not lines:
        check("client audit ran", False, audit.stderr.strip()[:400])
    else:
        passed += sum(1 for ln in lines if ln.startswith("PASS"))
        failed += sum(1 for ln in lines if ln.startswith("FAIL"))

    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
