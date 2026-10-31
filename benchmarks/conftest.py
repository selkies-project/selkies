# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""CPU benchmarks of the pure-Python hot paths, measured by CodSpeed.

They import the source tree directly, as the unit tier does, so they need the
runtime dependencies `scripts/ci/unit-deps.py` lists plus `pytest-codspeed`,
and neither pixelflux nor pcmflux:

    python -m pytest benchmarks --codspeed
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "src"))
