#!/usr/bin/env python3
"""When the client puts pointer motion on the wire.

A sample has to leave as it arrives, from pointerrawupdate where the engine
fires it, and never wait longer than the send interval, while a 1000 Hz mouse
is thinned to one message per interval and a transport that is not draining
gets one per frame. The checks drive the client's own handlers on a virtual
clock and read the messages it sends, in tests/tools/pointer_cadence_audit.mjs,
because the path under test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "pointer_cadence_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the pointer cadence audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [pointer-cadence] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[pointer-cadence] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
