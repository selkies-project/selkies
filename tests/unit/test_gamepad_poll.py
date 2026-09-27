#!/usr/bin/env python3
"""How the client reads gamepads.

The Gamepad API has no input event, so pads are polled, and every millisecond
between polls is latency every press pays; the on-screen touch gamepad, which
knows when it changes, says so and is read at once. A stick's rest noise is cut
by its distance from center, not axis by axis. The checks drive the client's
own poller over a stubbed `navigator.getGamepads`, in
tests/tools/gamepad_poll_audit.mjs, because the path under test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "gamepad_poll_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the gamepad poll audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [gamepad-poll] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[gamepad-poll] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
