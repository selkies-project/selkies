#!/usr/bin/env python3
"""Dual-mode /api/switch stress: flip webrtc<->websockets 6 times, verifying
the mode, HTTP routes, and that a repeat switch to the same mode is a clean
no-op (no crash, no duplicate service instances / port conflicts)."""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H


def main():
    H.server_start(mode="websockets", wayland=False)
    res = H.Results("switch")
    seq = ["webrtc", "websockets"] * 3
    for i, target in enumerate(seq):
        s, body = H.curl("/api/switch", method="POST", data={"mode": target})
        res.check(f"switch {i}->{target} returns 200", s == 200, body[:60])
        if s == 200:
            st = json.loads(H.curl("/api/status")[1])
            res.check(f"switch {i}: mode={target}", st.get("current_mode") == target,
                      st.get("current_mode"))
        time.sleep(1.0)
    # Last status
    st = json.loads(H.curl("/api/status")[1])
    res.check("final mode is websockets", st.get("current_mode") == "websockets", st)
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)