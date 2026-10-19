#!/usr/bin/env python3
"""What a Chromium Ctrl+V sends ahead of its V.

Chromium reads the local clipboard when the window gains focus, but the
clipboard also changes while the window keeps focus, and the chord would
then paste what the last focus read sent. The chord's keydown therefore
reads the clipboard itself and its V waits for that send. The rule is
JavaScript, so the checks live in tests/tools/clipboard_paste_chord_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "clipboard_paste_chord_audit.mjs")

node = shutil.which("node")
if not node:
    print("SKIP node not found, so the clipboard paste-chord audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [clip-paste-chord] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[clip-paste-chord] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
