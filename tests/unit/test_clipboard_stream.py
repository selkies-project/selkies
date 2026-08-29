#!/usr/bin/env python3
"""The clipboard worker's multipart accumulation.

A download reaches the worker one message at a time, so the page never holds
the whole base64 payload — which puts the chunk-boundary handling and the
per-transfer state in the worker. The rules are JavaScript, so the checks live
in tests/tools/clipboard_stream_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "clipboard_stream_audit.mjs")

node = shutil.which("node")
if not node:
    print("SKIP node not found, so the clipboard stream audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [clip-stream] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[clip-stream] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
