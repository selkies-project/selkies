# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Selkies package root.

Appends the package directory to `sys.path` so vendored modules that use
bare (top-level) imports resolve whether the code runs as an installed
package or straight from a source checkout.
"""

import os
import sys
sys.path.append(os.path.dirname(os.path.abspath(__file__)))
