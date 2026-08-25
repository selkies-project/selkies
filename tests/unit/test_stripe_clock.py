#!/usr/bin/env python3
"""When a striped codec's frame counts as whole on the client.

The striped paths composite partial-height stripes, so a frame is only proven
complete by the arrival of the next frame id -- a capture period later.
Presenting on socket quiet instead puts the frame on screen in the tick its
stripes landed, which is worth a frame period of end-to-end latency, but the
quiet asked for has to clear the gaps that open inside a frame or half a frame
reaches the screen. The rule is JavaScript, so the checks live in
tests/tools/stripe_clock_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "stripe_clock_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce the present rule as checked.
    print("SKIP node not found, so the stripe clock audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [stripe-clock] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[stripe-clock] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
