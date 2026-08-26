#!/usr/bin/env python3
"""What the apps panel shows while the server is still running a command.

Installs and removes are applied optimistically, so between the click and the
server's answer the panel has to say the action is running, and the answer has
to settle it either way: a clean exit clears the running state, a failure also
rolls the optimistic update back. The checks live in
tests/tools/app_commands_audit.mjs, because the path under test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "app_commands_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the app command audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [app-commands] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[app-commands] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
