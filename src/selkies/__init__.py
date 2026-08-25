# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Selkies package root.

Appends the package directory to `sys.path` so vendored modules that use
bare (top-level) imports resolve whether the code runs as an installed
package or straight from a source checkout, and publishes `__version__`.
"""

import os
import re
import sys
from importlib.metadata import PackageNotFoundError
from importlib.metadata import version as _distribution_version

sys.path.append(os.path.dirname(os.path.abspath(__file__)))


def _read_version() -> str:
    """Resolve the version of the running package from its one source.

    The installed distribution's metadata is authoritative: every channel
    (wheel, native package, conda package, AppImage) stamps the version of
    pyproject.toml into it at build time. A source checkout that is not
    installed has no metadata, so its pyproject.toml is read instead;
    anything else reports "unknown".
    """
    try:
        return _distribution_version("selkies")
    except PackageNotFoundError:
        pass
    checkout = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    try:
        with open(os.path.join(checkout, "pyproject.toml"), encoding="utf-8") as f:
            match = re.search(r'^version\s*=\s*"([^"]+)"', f.read(), re.MULTILINE)
    except OSError:
        match = None
    return match.group(1) if match else "unknown"


__version__ = _read_version()
