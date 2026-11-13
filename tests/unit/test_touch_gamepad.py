#!/usr/bin/env python3
"""What the on-screen touch gamepad reports.

The overlay turns touches into a standard-layout pad the page reads through
`navigator.getGamepads`: buttons hit tested per cluster so a sliding finger
re-targets, sticks whose short, still tap clicks L3 or R3, analog triggers, a
layout the user picks and the page keeps, and a pad that exists only while the
overlay is shown. The checks drive the module the dashboard bundles over a
stand-in page, with the core's own poller reading the pad beside them, in
tests/tools/touch_gamepad_audit.mts, because the path under test is TypeScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "touch_gamepad_audit.mts")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the touch gamepad audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [touch-gamepad] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[touch-gamepad] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
