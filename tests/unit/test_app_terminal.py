#!/usr/bin/env python3
"""Which terminal the apps panel launches applications in.

A terminal is the one thing this needs that no two systems agree on, and a
built-in list of names is the worst way to find it: it is right for the image it
was written against and empty everywhere else. So the answer comes from whoever
already has one — the setting, `TERMINAL`, the desktop's own `xdg-terminal-exec`
— and the list is only what is left when nobody does. The flag a terminal takes
its command after is the part that cannot be asked for: a desktop entry does not
carry it, so it is tabled, with the flag all but a handful use as the default.
"""
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
import helpers as H  # noqa: E402

from selkies import input_handler as IH  # noqa: E402
from selkies.settings import settings  # noqa: E402

res = H.Results("app-terminal")


def resolved(installed=(), configured="", terminal_env=None, session="x11") -> str:
    """What `app_terminal` answers with those stand-ins in place.

    Args:
        installed: Command names that exist on this stand-in PATH.
        configured: The `app_terminal` setting.
        terminal_env: The TERMINAL environment variable, or None for unset.
        session: The windowing system the session's applications run on.
    """
    handler = IH.WebRTCInput.__new__(IH.WebRTCInput)
    handler.app_session = lambda: {"type": session}
    saved_which, saved_setting = IH.shutil.which, settings.app_terminal
    saved_env = os.environ.get("TERMINAL")
    IH.shutil.which = lambda name: f"/usr/bin/{name}" if name in installed else None
    settings.app_terminal = configured
    os.environ.pop("TERMINAL", None)
    if terminal_env is not None:
        os.environ["TERMINAL"] = terminal_env
    try:
        return handler.app_terminal()
    finally:
        IH.shutil.which, settings.app_terminal = saved_which, saved_setting
        os.environ.pop("TERMINAL", None)
        if saved_env is not None:
            os.environ["TERMINAL"] = saved_env


res.check("what the deployment configured is taken verbatim",
          resolved(installed=("foot",), configured="myterm --run") == "myterm --run")
res.check("TERMINAL is read the way other launchers read it",
          resolved(installed=("alacritty", "xterm"), terminal_env="alacritty") == "alacritty -e")
res.check("a TERMINAL that carries its own flag keeps it",
          resolved(installed=("myterm",), terminal_env="myterm --here") == "myterm --here")
res.check("a TERMINAL that is not installed is not used",
          resolved(installed=("xterm",), terminal_env="absent") == "xterm -e")
res.check("the resolvers a system provides come before any list",
          resolved(installed=("xdg-terminal-exec", "xterm")) == "xdg-terminal-exec"
          and resolved(installed=("x-terminal-emulator", "xterm")) == "x-terminal-emulator -e",
          resolved(installed=("x-terminal-emulator", "xterm")))
res.check("an unlisted terminal is assumed to take the usual flag",
          IH.terminal_command("someterm") == "someterm -e"
          and IH.terminal_command("/opt/bin/foot") == "/opt/bin/foot",
          IH.terminal_command("/opt/bin/foot"))
res.check("each session's own terminal is preferred",
          resolved(installed=("st", "foot", "xterm"), session="wayland") == "foot"
          and resolved(installed=("st", "foot", "xterm")) == "st")
res.check("the list is walked in order when nothing else answers",
          resolved(installed=("konsole", "xterm")) == "konsole -e")
res.check("a Wayland session can still fall back to an X11 terminal",
          resolved(installed=("st", "xterm"), session="wayland") == "st")
res.check("no terminal at all is answered honestly", resolved() is None)

sys.exit(0 if res.summary() else 1)
