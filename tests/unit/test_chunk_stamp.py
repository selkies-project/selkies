#!/usr/bin/env python3
"""The timestamps a client's decoder chunks carry.

A decoded frame is matched to its chunk by the timestamp WebCodecs hands back
in whole microseconds (the decode time and arrival the stats report), so each
stamp is whole microseconds, and no two chunks share one: WebKit's worker
clock reads whole milliseconds, and WebKit gives frames of chunks sharing a
timestamp other times entirely. The rule is JavaScript, so the checks live in
tests/tools/chunk_stamp_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "chunk_stamp_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the chunk stamp audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [chunk-stamp] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[chunk-stamp] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
