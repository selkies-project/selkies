#!/usr/bin/env python3
"""When the webcam uplink gives up on a codec.

A frame offered to a busy encoder is dropped rather than queued, so a codec the
engine cannot encode in real time never shows a growing queue: it shows a core
at full tilt and a receiver watching a fraction of the camera. The share of
frames dropped is what moves the uplink to the next codec and finally to the
JPEG rung, which encodes natively. Deciding it from the live camera is the
point -- a codec's cost depends on what the sensor is showing, and a synthetic
probe frame ranks codecs by how cheap its own content was. The rule is
JavaScript, so the checks live in tests/tools/encode_pace_audit.mjs.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "encode_pace_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce the ladder as checked.
    print("SKIP node not found, so the encode pace audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [encode-pace] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[encode-pace] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
