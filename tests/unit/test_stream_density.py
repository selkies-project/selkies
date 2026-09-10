#!/usr/bin/env python3
"""What density a page streams at, and what scale it publishes for its neighbours.

Every page streams at the density of the screen it is on, so a display is one
stream pixel per device pixel wherever it is shown. A page publishes the scale
of its box with the layout, for the cross-display drag, by measuring the box it
draws the stream in, which only answers the question while the box is showing
the stream the server realized: one still holding the stream from before a
resize measures that stream's ratio. With HiDPI off the desktop
is left at 96 DPI and the UI-scaling pick divides the resolution asked for
instead, the browser stretching the stream back by it, so the pick decides what
a page requests as much as the display's own scaling does. The rules are
JavaScript, so the checks live in tests/tools/stream_density_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "stream_density_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce the rule as checked.
    print("SKIP node not found, so the stream density audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [stream-density] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[stream-density] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
