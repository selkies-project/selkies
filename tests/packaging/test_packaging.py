#!/usr/bin/env python3
"""The native packaging scripts stage cleanly against a read-only checkout.

infra/packaging/*.sh run in CI inside real distribution containers, which proves their produced packages. Against this
checkout tests/packaging/simulate.sh catches read-only-mount, staging-path, and version-form failures without a
container runtime.
"""
import os
import subprocess
import sys

SIMULATE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "simulate.sh")


def main() -> bool:
    """Run the native packaging simulation, streaming its checks in real time.

    Returns:
        Whether every distribution's staging script succeeded.
    """
    run = subprocess.run(["bash", SIMULATE])
    return run.returncode == 0


def test_packaging() -> None:
    """Check every distribution's read-only staging simulation."""
    assert main()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
