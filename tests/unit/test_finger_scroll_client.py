#!/usr/bin/env python3
"""What a touchpad's scroll puts on the wire.

Where the session takes a finger's scroll (Wayland), a wheel the page
classifies as a touchpad sends its travel in stream pixels and an end once it
pauses; a wheel's notches, line deltas, and a session that does not take one
keep the notch pulses. The checks drive the client's own Input on a virtual
clock, in tests/tools/finger_scroll_audit.mjs, because the path under test is
JavaScript; the server half is test_finger_scroll.py.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "finger_scroll_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the finger scroll audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [finger-scroll] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[finger-scroll] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
