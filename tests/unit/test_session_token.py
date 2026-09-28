#!/usr/bin/env python3
"""Where the page URL's session token comes from and where the client sends it.

A token in the fragment reaches the server only as a header, a subprotocol, or
the cookie; a query token keeps its URL carriers; the display and sharing
keyword reads the same beside either. The checks live in
tests/tools/session_token_audit.mjs, because the path under test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "session_token_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the session token audit cannot run", flush=True)
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [session-token] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[session-token] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
