# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Selkies's entry in jupyter-server-proxy's `jupyter_serverproxy_servers` group.

jupyter-server-proxy calls every entry of that group for the server it
proxies. The configuration is the server's own: `selkies session
--jupyter-config` prints it with the executable and the web client's icon,
which it writes out of the executable into the user's data directory, so this
only runs the executable this wheel installed and returns its entry, and the
two never describe different launchers.

The group is the one registration a `pip install` can make, which is why this
module, its package, and the entry point in `pyproject.toml` are Python:
jupyter-server-proxy loads each entry as a Python callable, Jupyter Server
takes only `ServerApp.jpserver_extensions` from the `jupyter_server_config.d`
drop-in directories, and `jupyter_server_config.json`, which would carry
`ServerProxy.servers`, is the user's own file.
"""
import json
import shutil
import subprocess
from importlib import metadata
from typing import Any, Dict


def _executable() -> str:
    """The `selkies` executable this distribution installed.

    Read from the distribution's record rather than from the interpreter's
    scripts directory, which is not where a `--user` install puts it; the
    first `selkies` on PATH where the record names none.
    """
    for path in metadata.distribution("selkies").files or ():
        if path.name == "selkies" and path.parent.name == "bin":
            return str(path.locate())
    return shutil.which("selkies") or "selkies"


def jupyter() -> Dict[str, Any]:
    """The server process jupyter-server-proxy starts a desktop with.

    Raises:
        subprocess.CalledProcessError: the executable could not print its configuration.
    """
    printed = subprocess.run([_executable(), "session", "--jupyter-config"],
                             check=True, capture_output=True, text=True).stdout
    return json.loads(printed)["ServerProxy"]["servers"]["selkies"]
