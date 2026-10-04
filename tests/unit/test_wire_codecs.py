#!/usr/bin/env python3
"""The codec string a page reads off an AV1 key frame's sequence header.

Every operating point is read past, not only the first, and the header as it
stands, since AV1 has no emulation prevention. The parser is JavaScript in the
web core, so the checks live in tests/tools/wire_codecs_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "wire_codecs_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the wire codecs audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [wire-codecs] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[wire-codecs] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
