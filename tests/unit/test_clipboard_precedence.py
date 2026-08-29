#!/usr/bin/env python3
"""Which local-to-server clipboard send wins.

The image upload and the focus-driven local sync both write the session
clipboard, and choosing a file raises the very focus event that fires the
second one -- so without precedence the upload lands and the client's own
clipboard lands on top of it, which reads as the button doing nothing. The
rule is JavaScript, so the checks live in
tests/tools/clipboard_precedence_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "clipboard_precedence_audit.mjs")

node = shutil.which("node")
if not node:
    print("SKIP node not found, so the clipboard precedence audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [clip-precedence] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[clip-precedence] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
