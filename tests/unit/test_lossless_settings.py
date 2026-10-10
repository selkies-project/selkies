#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Validate opt-in defaults and the dashboards' preference/capability contract."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
base_env = {key: value for key, value in os.environ.items() if not key.startswith("SELKIES_")}
base_env["PYTHONPATH"] = str(ROOT / "src")
code = ("import json; from selkies.settings import settings, STREAM_SETTINGS; "
        "print(json.dumps([settings.lossless_static_refinement, "
        "settings.was_provided('lossless_static_refinement'), "
        "'lossless_static_refinement' in STREAM_SETTINGS]))")
for value, expected in [(None, [[False, False], False, True]),
                        ("true", [[True, False], True, True]),
                        ("false|locked", [[False, True], True, True])]:
    env = dict(base_env)
    if value is not None:
        env["SELKIES_LOSSLESS_STATIC_REFINEMENT"] = value
    result = subprocess.run([sys.executable, "-c", code], env=env, capture_output=True,
                            text=True, timeout=30, check=True)
    actual = json.loads(result.stdout.strip().splitlines()[-1])
    if actual != expected:
        raise AssertionError((value, actual, expected))
    print(f"PASS [lossless-settings] server opt-in {value!r}", flush=True)

node = shutil.which("node")
if node is None:
    print("SKIP node unavailable; dashboard state audit did not run", flush=True)
    sys.exit(77)
result = subprocess.run([node, str(ROOT / "tests/tools/lossless_settings_audit.mjs")],
                        text=True, timeout=30)
sys.exit(result.returncode)
