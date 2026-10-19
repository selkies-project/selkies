# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""The tools `selkies <command>` runs besides the server.

Each also installs as a `selkies-<command>` console script, kept so the
invocations written before these existed go on working; `selkies <command>`
is the form to use, and reaches every tool from the one executable, where a
packaging carries nothing else.
"""

import importlib
import sys
from typing import Dict, List, Tuple

#: command: (module, function, what it does), for `selkies --help` as well.
COMMANDS: Dict[str, Tuple[str, str, str]] = {
    "session": ("selkies.session", "main", "start a desktop session and Selkies streaming it"),
    "resize": ("selkies.display_utils", "entrypoint", "resize the X display to WxH"),
    "gpu-probe": ("selkies.gpu_probe", "main", "report the GPU a session renders and encodes on"),
}


def help_epilog() -> str:
    """The commands, as the server's --help lists them after its own options."""
    width = max(len(name) for name in COMMANDS)
    lines = [f"  selkies {name:<{width}}  {COMMANDS[name][2]}" for name in COMMANDS]
    return "commands:\n" + "\n".join(lines)


def run(argv: List[str]) -> None:
    """Run the command ``argv[1]`` names and exit with its status; return
    when it names none, leaving ``argv`` to the server.

    The command sees the arguments after its name, under the name it was
    called by, as its console script would.
    """
    if len(argv) < 2 or argv[1] not in COMMANDS:
        return
    name = argv[1]
    module, function, _ = COMMANDS[name]
    argv[0:2] = [f"selkies {name}"]
    sys.exit(getattr(importlib.import_module(module), function)())
