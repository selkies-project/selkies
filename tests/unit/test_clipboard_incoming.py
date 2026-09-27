#!/usr/bin/env python3
"""When a session copy is written to the local clipboard.

Engines decide whether a clipboard write may happen at the moment it is asked
for, and a large image is still crossing the link by the time the user has left
to paste it, so the write is asked for at the payload's first frame; a repeat or
a superseded payload withdraws it, and a refused one waits for the next gesture
only while nothing newer was asked for. The rules are JavaScript, so the checks
live in tests/tools/clipboard_incoming_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "clipboard_incoming_audit.mjs")

node = shutil.which("node")
if not node:
    print("SKIP node not found, so the clipboard incoming audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [clip-incoming] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[clip-incoming] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
