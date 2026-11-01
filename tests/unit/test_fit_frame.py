#!/usr/bin/env python3
"""Which decoded pictures the client cuts to the stream's size.

A hardware encoder may code a larger picture than the stream's and name the
stream's size in the AV1 render size, which Chromium's decoders do not apply,
so the client crops a picture larger than the size its decoder was configured
for and passes every other one through. The rule is JavaScript, so the checks
live in tests/tools/fit_frame_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "fit_frame_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the frame fit audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [fit-frame] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[fit-frame] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
