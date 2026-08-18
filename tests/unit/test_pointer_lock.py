#!/usr/bin/env python3
"""How the client asks for pointer lock, and what it does when the engine says no.

Locked motion is relayed as relative motion and injected as relative motion, so
the OS acceleration curve the engine applies to movementX/Y lands on top of the
one the remote desktop applies. The client asks for the raw deltas instead, and
every engine on Linux and Android refuses that with NotSupportedError -- which
must still end in a lock, must not be mistaken for a real failure, and must not
slip past the guards the fullscreen caller checked before it asked. The checks
live in tests/tools/pointer_lock_audit.mjs, because the path under test is
JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "pointer_lock_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce that pointer lock behaves without having
    # looked at it.
    print("SKIP node not found, so the pointer lock audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [pointer-lock] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[pointer-lock] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
