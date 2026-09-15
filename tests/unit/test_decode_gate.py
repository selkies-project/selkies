#!/usr/bin/env python3
"""What the client's decoder does with a frame it cannot keep up with.

A decoder that lets a frame go breaks every frame predicting from it, and the
repair available depends on what the encoder says: where each frame names what
it predicts from, the client reports the one it dropped and the encoder
predicts past it, so nothing costs a key frame; where it names nothing, a key
frame is the only repair. The checks live in tests/tools/decode_gate_audit.mjs,
because the path under test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "decode_gate_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce that the gate behaves without having looked.
    print("SKIP node not found, so the decode gate audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [decode-gate] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[decode-gate] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
