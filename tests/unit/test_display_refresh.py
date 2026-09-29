#!/usr/bin/env python3
"""The display refresh a page measures, and the frame rate it asks for from it.

A stream matching its display's refresh gives every frame a refresh of its own,
so the page measures the refresh from its animation frames and asks the server
for that rate where the user chose it. The estimator, its snap to the whole and
NTSC rates, and the rate asked for are JavaScript, so the checks run on
synthetic animation frames in tests/tools/display_refresh_audit.mjs: timer
resolutions from 20 us to 1 ms, a software vsync's jitter, frames a busy page
missed, and a page hidden or throttled in the background.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "display_refresh_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the display refresh audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [display-refresh] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[display-refresh] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
