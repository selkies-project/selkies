#!/usr/bin/env python3
"""What a local copy goes to the session as.

An office application copying formatted text offers a picture of the selection
beside its markup, so a copy whose markup carries text is text, and a copied
picture, whose markup is only an img tag, stays the picture. The rule is
JavaScript, so the checks live in tests/tools/clipboard_local_read_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "clipboard_local_read_audit.mjs")

node = shutil.which("node")
if not node:
    print("SKIP node not found, so the clipboard local-read audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [clip-local-read] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[clip-local-read] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
