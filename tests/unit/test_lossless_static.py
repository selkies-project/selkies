#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Run deterministic browser-independent refinement ordering and ownership checks."""
import os
import shutil
import subprocess
import sys

AUDIT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                     "tools", "lossless_static_audit.mjs")
node = shutil.which("node")
if not node:
    print("SKIP node not found; lossless ordering audit cannot run", flush=True)
    sys.exit(77)
result = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
print(result.stdout, end="", flush=True)
if result.stderr:
    print(result.stderr, end="", file=sys.stderr, flush=True)
sys.exit(result.returncode)
