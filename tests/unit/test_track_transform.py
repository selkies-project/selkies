#!/usr/bin/env python3
"""What the client does with a worker's video track APIs, run without a browser.

`VideoTrackGenerator` and `MediaStreamTrackProcessor` are exposed to dedicated
workers alone, so a page that asks for either sees `undefined` whatever the
engine, and WebKit gates both on a preference whose default is per platform:
`MediaStreamTrackProcessingEnabled` is on under `PLATFORM(COCOA)` and off in
every other build of the same engine, including the one the browser suites
launch, which offers no way to turn it on. The paths that carry the whole video
sink and the whole camera uplink would therefore go unexercised.

The checks run the client's own worker sources against stubs holding those
interfaces to their IDL: a generator constructed with no arguments, whose
writer comes from `writable` and whose `track` reaches the page in the transfer
list, and a processor constructed from a dictionary whose `readable` is the
frame source. They live in tests/tools/track_generator_probe.mjs, because the
path under test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PROBE = os.path.join(TESTS, "tools", "track_generator_probe.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the probe is the whole suite.
    print("SKIP node not found, so the track transform probe cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, PROBE], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [track-transform] the probe ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[track-transform] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
