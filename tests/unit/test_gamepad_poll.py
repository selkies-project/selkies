#!/usr/bin/env python3
"""How the client reads gamepads.

The Gamepad API has no input event, so pads are polled, and every millisecond
between polls is latency every press pays; the on-screen touch gamepad, which
knows when it changes, says so and is read at once. A stick's rest noise is cut
by its distance from center, not axis by axis. A page holding several slots (a
token's list) drives one with each pad. The checks drive the client's own
poller over a stubbed `navigator.getGamepads`, in
tests/tools/gamepad_poll_audit.mjs and gamepad_slots_audit.mjs, because the
path under test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDITS = [os.path.join(TESTS, "tools", name) for name in ("gamepad_poll_audit.mjs", "gamepad_slots_audit.mjs")]

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audits are the whole suite.
    print("SKIP node not found, so the gamepad audits cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

lines = []
status = 0
for audit in AUDITS:
    r = subprocess.run([node, audit], capture_output=True, text=True, timeout=120)
    ran = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
    for line in ran:
        print(line, flush=True)
    if not ran:
        print(f"FAIL  [gamepad] {os.path.basename(audit)} ran  {r.stderr.strip()[:400]}", flush=True)
        status = 1
    lines += ran
    status = status or r.returncode

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[gamepad-poll] {passed}/{len(lines)} passed")
sys.exit(status)
