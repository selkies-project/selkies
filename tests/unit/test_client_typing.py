#!/usr/bin/env python3
"""What the client puts on the wire for typed, pasted and re-typed text.

A capital has to travel as its own keysym. Sending Shift plus the lowercase
keysym instead relies on the X keymap binding Shift as a real modifier, which a
keymap-less server does not do -- so capitals arrive lowercase and nothing
reports an error. The soft-input and text-echo paths share one implementation
here so they cannot diverge again, which is how the defect survived a fix to
only one of them. The checks live in tests/tools/client_typing_audit.mjs,
because the paths under test are JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "client_typing_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce that every path types correctly without
    # having looked at one.
    print("SKIP node not found, so the client typing audit cannot run", flush=True)
    sys.exit(77)  # helpers.SKIP_EXIT, without importing the e2e helper module

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [client-typing] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[client-typing] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
