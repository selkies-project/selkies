#!/usr/bin/env python3
"""What the client lets go of when its page stops getting input.

A press whose release goes to another tab or app (a tab switched mid-press, a
touch the system took for a gesture) left the remote button down: every motion
after it dragged, and the next press did nothing; a touch whose end went
elsewhere counted as a finger down for good, so every tap after it was a
two-finger gesture that scrolled and never clicked. Hidden, blurred or leaving,
the page releases every key, button and touch it holds, and a touch no longer
on the surface is forgotten when the next lands. The checks live in
tests/tools/held_input_audit.mjs, because the path under test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "held_input_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce that held input is released without having
    # looked at it.
    print("SKIP node not found, so the held input audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [held-input] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[held-input] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
