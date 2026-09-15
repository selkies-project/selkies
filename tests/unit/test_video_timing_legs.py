#!/usr/bin/env python3
"""The video-timing extension a frame leaves the sender with: from the capture
library's stamps, the encode legs are the encode's own start and end and the
packetization leg the frame's whole age; without stamps, the encode legs are
unknown and the packetization leg is the frame's time in the sender. Every leg
is milliseconds from the capture, clamped to the field.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc.rtcrtpsender import video_timing_legs  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [video-timing] {label}  {detail}", flush=True)


MS = 1_000_000
capture = 5_000 * MS
got = video_timing_legs((capture, capture + 2 * MS, capture + 9 * MS), capture + 12 * MS, 99)
check("stamped: encode legs and the frame's age from the capture", got == (0x01, 2, 9, 12, 12, 0, 0), got)
got = video_timing_legs(None, capture, 7)
check("unstamped: encode legs unknown, the sender's own age", got == (0x01, 0, 0, 7, 7, 0, 0), got)
got = video_timing_legs((0, 0, 0), capture, 7)
check("zero stamps read as unstamped", got == (0x01, 0, 0, 7, 7, 0, 0), got)
got = video_timing_legs((capture, capture + 70_000 * MS, capture + 80_000 * MS), capture + 90_000 * MS, 0)
check("legs clamp to the 16-bit field", got == (0x01, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0, 0), got)
got = video_timing_legs((capture, capture - MS, capture), capture, 0)
check("a stamp before the capture reads as zero, not negative", got[1] == 0 and got[2] == 0, got)

print(f"[video-timing] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
