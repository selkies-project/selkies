#!/usr/bin/env python3
"""When an attached keyboard hides the on-screen keyboard button.

A tablet with a keyboard attached keeps the system's on-screen keyboard down,
so the dashboards' button that pops it goes once a key only a keyboard has is
pressed, and a touch after an idle spell brings it back. The rule is
JavaScript, so the checks live in tests/tools/hardware_keyboard_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "hardware_keyboard_audit.mjs")

node = shutil.which("node")
if not node:
    print("SKIP node not found, so the hardware keyboard audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [hw-keyboard] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[hw-keyboard] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
