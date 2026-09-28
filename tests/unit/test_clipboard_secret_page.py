#!/usr/bin/env python3
"""A secret the session copied, on the page.

Its preview reaches the dashboards masked, never as its text; written locally,
it is taken back 60 seconds later, or as soon as the session's clipboard moves
on, but only while the local clipboard still holds it, and only where the
engine lets a page check that without a gesture or a prompt. The rules are
JavaScript, so the checks live in tests/tools/clipboard_secret_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "clipboard_secret_audit.mjs")

node = shutil.which("node")
if not node:
    print("SKIP node not found, so the clipboard secret audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [clip-secret-page] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[clip-secret-page] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
