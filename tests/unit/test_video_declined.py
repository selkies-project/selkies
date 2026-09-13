#!/usr/bin/env python3
"""A WebRTC page whose answer declined the codec a held encoder streams is
told so over its channel, once the channel is open, and never otherwise.
"""
import json
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.argv = ["selkies"]

from selkies.webrtc_engine import RTCApp  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [video-declined] {label}  {detail}", flush=True)


class Channel:
    def __init__(self, state: str) -> None:
        self.readyState = state
        self.sent: list = []

    def send(self, data: str) -> None:
        self.sent.append(json.loads(data))


rtc = RTCApp.__new__(RTCApp)
rtc.peer_connections = {"declined": {"video_declined": "video/H265"}, "fine": {}}

closed = Channel("connecting")
rtc._send_video_declined(closed, "declined")
check("nothing is sent before the channel opens", closed.sent == [], closed.sent)
opened = Channel("open")
rtc._send_video_declined(opened, "declined")
check("an open channel carries the declined codec as a system action",
      opened.sent == [{"type": "system", "data": {"action": "video_declined,video/H265"}}], opened.sent)
rtc._send_video_declined(opened, "fine")
rtc._send_video_declined(opened, "unknown")
check("a peer whose codec was taken, or none at all, hears nothing", len(opened.sent) == 1, opened.sent)

print(f"[video-declined] {passed}/{passed + failed} passed", flush=True)
sys.exit(1 if failed else 0)
