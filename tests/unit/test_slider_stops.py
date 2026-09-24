#!/usr/bin/env python3
"""The stops the settings sliders step through for frame rate, bitrate, and CRF.

Both dashboards move those sliders over one shared list per setting, so a stop
owns a wide band of the track and a finger or a small mouse move lands on a
value worth choosing rather than a frame or two away from it. The lists and the
helpers that clip them to a server's span and place a value on them are
JavaScript, so the checks live in tests/tools/slider_stops_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "slider_stops_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the slider stops audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [slider-stops] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[slider-stops] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
