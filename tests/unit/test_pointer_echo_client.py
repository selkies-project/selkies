#!/usr/bin/env python3
"""Where trackpad mode draws the cursor.

The page asks the server to echo the pointer, draws the cursor at the echo
moved on by the deltas it sent past the message the echo includes, hides it
while the echo names another display, and has the cursor composited into the
video instead where no echo comes. The checks drive the client's own Input on
a virtual clock, in tests/tools/pointer_echo_audit.mjs, because the path under
test is JavaScript; the server half is test_pointer_echo.py.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "pointer_echo_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the pointer echo audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [pointer-echo-client] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[pointer-echo-client] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
