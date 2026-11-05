#!/usr/bin/env python3
"""Global and command-local help reach their respective handlers."""
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H


def main() -> bool:
    """Check help behavior through the actual server executable."""
    res = H.Results("commands")
    for label, args, wanted, unwanted in (
        ("global", ["--help"], "Selkies WebSocket Streaming Server", ""),
        ("session", ["session", "--help"], "Start a desktop session and Selkies streaming it",
         "Selkies WebSocket Streaming Server"),
        ("resize", ["resize", "--help"], "usage: selkies resize <WxH>", "Selkies WebSocket Streaming Server"),
    ):
        run = subprocess.run([H.SERVER, *args], capture_output=True, text=True, timeout=30)
        output = run.stdout + run.stderr
        res.check(
            f"{label} help uses the local handler",
            run.returncode == 0 and wanted in output and (not unwanted or unwanted not in output),
            output.splitlines()[0] if output.splitlines() else run.returncode,
        )
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
