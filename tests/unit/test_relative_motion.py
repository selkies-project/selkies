#!/usr/bin/env python3
"""What the client puts on the wire for relative motion.

Pointer lock and the trackpad both produce deltas rather than positions, and a
delta only reaches the remote pointer if it survives being scaled onto the
stream's pixels and quantized. Browsers report movement in whole CSS pixels and
carry their own remainder, so on any client whose scale is not a whole number
the deltas arrive as a stream that only adds up over several events: quantizing
each one on its own turns that into a sensitivity error that grows with speed,
drops slow motion entirely, and drifts on motion that should cancel out. The
checks live in tests/tools/relative_motion_audit.mjs, because the path under
test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "relative_motion_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce that relative motion behaves without having
    # looked at it.
    print("SKIP node not found, so the relative motion audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [relative-motion] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[relative-motion] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
