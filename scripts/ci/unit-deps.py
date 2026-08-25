#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Prints the requirements the unit tier installs, one per line.

The tier imports the source tree directly, so every module-level import
under `selkies` has to resolve; listing them by hand in the workflow turns
each new dependency into a red run. This reads the runtime dependencies from
`pyproject.toml` and drops only the capture stack (pixelflux, pcmflux), which
the suites stub and whose main-HEAD wheels the integration tier alone
installs. Regex rather than a TOML parser because 3.9 has no `tomllib`
and the tier installs nothing before running this.

Usage: python -m pip install pytest -r <(python scripts/ci/unit-deps.py)
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CAPTURE_STACK = ("pixelflux", "pcmflux")

with open(os.path.join(ROOT, "pyproject.toml"), encoding="utf-8") as f:
    block = re.search(r"^dependencies = \[(.*?)^\]", f.read(), re.S | re.M)
if block is None:
    sys.exit("pyproject.toml: no [project] dependencies block")
for dep in re.findall(r'^\s*"([^"]+)"', block.group(1), re.M):
    if not dep.startswith(CAPTURE_STACK):
        print(dep)
