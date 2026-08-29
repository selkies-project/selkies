#!/usr/bin/env python3
"""The digest the clipboard worker takes while it codes a payload.

It stands in for the payload's bytes wherever a signature is taken, so the
page never walks a clipboard of any size to decide whether it changed. That
only holds while the worker's digest reproduces the page's exactly, over any
chunking. The rules are JavaScript, so the checks live in
tests/tools/clipboard_digest_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "clipboard_digest_audit.mjs")

node = shutil.which("node")
if not node:
    print("SKIP node not found, so the clipboard digest audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [clip-digest] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[clip-digest] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
