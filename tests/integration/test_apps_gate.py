#!/usr/bin/env python3
"""The apps panel is published only where its runner can work.

Its buttons are `selkies-proot` calls over the command channel, so a session
whose runner is missing, refuses the environment (proot cannot ptrace there)
or has no command channel at all can offer nothing that succeeds. The server
answers that by publishing ui_sidebar_show_apps as effective availability,
which is what both dashboards gate the panel on.
"""
import asyncio
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets


def runner_stub(path: str, check_exit: int) -> None:
    """Write a `selkies-proot` stand-in whose `check` exits as told.

    Args:
        path: File to create; its directory goes on the server's PATH.
        check_exit: Status the wrapper's `check` answers with.
    """
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        fh.write('#!/bin/sh\n'
                 'if [ "$1" = check ]; then\n'
                 '  [ %d -eq 0 ] || echo "selkies-proot: proot cannot ptrace here" >&2\n'
                 '  exit %d\n'
                 'fi\n'
                 'exit 0\n' % (check_exit, check_exit))
    os.chmod(path, 0o755)


async def published_apps_setting() -> object:
    """The `ui_sidebar_show_apps` value in the connect-time settings payload."""
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as ws:
        for _ in range(20):
            msg = await asyncio.wait_for(ws.recv(), timeout=10)
            if isinstance(msg, str) and msg.startswith("{"):
                obj = json.loads(msg)
                if obj.get("type") == "server_settings":
                    return obj["settings"].get("ui_sidebar_show_apps", {}).get("value")
    return None


def probe(res: "H.Results", label: str, expect: bool, **env) -> None:
    """Start a server under `env` and check what it publishes for the panel."""
    H.server_start(mode="websockets", wayland=False, extra_env=env)
    try:
        got = asyncio.run(published_apps_setting())
    finally:
        H.server_stop()
    res.check(f"apps panel {'shown' if expect else 'hidden'}: {label}",
              got is expect, f"ui_sidebar_show_apps={got!r}")


def run() -> "H.Results":
    """Publish the panel for a working runner and withhold it otherwise."""
    res = H.Results("apps-gate")
    bin_ok = os.path.join(H.WORKDIR, "apps-ok-bin")
    bin_bad = os.path.join(H.WORKDIR, "apps-bad-bin")
    empty = os.path.join(H.WORKDIR, "apps-none-bin")
    runner_stub(os.path.join(bin_ok, "selkies-proot"), 0)
    runner_stub(os.path.join(bin_bad, "selkies-proot"), 1)
    os.makedirs(empty, exist_ok=True)
    base_path = os.environ.get("PATH", "")

    probe(res, "runner reports it can run here", True,
          SELKIES_COMMAND_ENABLED="true", PATH=bin_ok + os.pathsep + base_path)
    probe(res, "runner reports the environment denies it", False,
          SELKIES_COMMAND_ENABLED="true", PATH=bin_bad + os.pathsep + base_path)
    probe(res, "no runner installed", False,
          SELKIES_COMMAND_ENABLED="true", PATH=empty)
    probe(res, "command channel disabled", False,
          SELKIES_COMMAND_ENABLED="false", PATH=bin_ok + os.pathsep + base_path)
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(1 if run().failed() else 0)
