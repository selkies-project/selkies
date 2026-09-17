#!/usr/bin/env python3
"""What a dashboard is told of the session, and when.

The page's collector asks the server for the moving figures only while a
dashboard has its stats open, never for a viewer, and again on a fresh
connection, and a status row warns where the session fell short of what it
asked for and never for a choice. The rules are JavaScript, shared by both
cores and both dashboards, so the checks live in
tests/tools/stream_stats_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "stream_stats_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce the rule as checked.
    print("SKIP node not found, so the stream stats audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [stream-stats] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[stream-stats] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
