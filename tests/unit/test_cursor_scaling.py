#!/usr/bin/env python3
"""Cursor size and hotspot follow the displayed stream in both transports.

The JavaScript audit drives Input with fitted and exact video/canvas sinks at
multiple densities and checks that cursor rendering does not alter input.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
node = shutil.which("node")
if not node:
    print("SKIP node not found, so the cursor scaling audit cannot run", flush=True)
    sys.exit(77)

result = subprocess.run([node, os.path.join(TESTS, "tools", "cursor_scaling_audit.mjs")],
                        capture_output=True, text=True, timeout=120)
lines = [line for line in result.stdout.splitlines() if line.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [cursor-scaling] audit ran  {result.stderr.strip()[:400]}", flush=True)
    sys.exit(1)
passed = sum(line.startswith("PASS") for line in lines)
print(f"[cursor-scaling] {passed}/{len(lines)} passed")
if result.returncode:
    print(result.stderr[-1000:], flush=True)
sys.exit(result.returncode)
