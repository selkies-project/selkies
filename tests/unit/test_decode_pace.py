#!/usr/bin/env python3
"""The frame rate a client whose decoder cannot keep up asks the server for.

A decoder behind the stream for three seconds in a row asks for a share of the
frames it turned out, and less again while it stays behind; keeping up, it asks
for more after a wait, takes a rise back when it falls behind soon after, and
waits twice as long before the next. The checks live in
tests/tools/decode_pace_audit.mjs, because the path under test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "decode_pace_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce that the pace behaves without having looked.
    print("SKIP node not found, so the decode pace audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [decode-pace] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[decode-pace] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
