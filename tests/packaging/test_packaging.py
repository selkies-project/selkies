#!/usr/bin/env python3
"""The native packaging scripts stage cleanly against a read-only checkout.

infra/packaging/*.sh run in CI inside real distro containers, which is where
their output is proven. What that cannot catch early is the class of defect that
kills a package build minutes in and only on one packager: a write into the
read-only /repo mount, a staging path that moved, a version spelled in a form
the packager rejects, or a command that ends up missing from PATH.
tests/packaging/simulate.sh reproduces exactly that, without a container
runtime, by rebasing the absolute paths into a sandbox and stubbing only the
tools that need root or a network.

The wheel it packages is a real one: reused from WHEEL_DIR or dist/ when a build
already produced it, and built here when not, so the suite has no precondition
to skip on.
"""
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

SIMULATE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "simulate.sh")
BUILT_WHEELS = os.path.join(H.WORKDIR, "packaging-wheel")


def has_wheel(directory: str) -> bool:
    return bool(directory) and os.path.isdir(directory) and any(
        name.startswith("selkies-") and name.endswith(".whl")
        for name in os.listdir(directory))


def wheel_dir() -> str:
    """A directory holding a selkies wheel, building one if none exists.

    The build is tried without isolation first, which needs no network when the
    interpreter running the tests already carries setuptools, and falls back to
    an isolated build for one that does not (3.12 onwards leaves setuptools out
    of a fresh virtualenv). The result is kept, so only the first run pays for
    it: what the packaging scripts exercise is installing a real wheel, not
    whichever revision built it.

    Raises:
        RuntimeError: Neither build produced a wheel.
    """
    for candidate in (os.environ.get("WHEEL_DIR", ""),
                      os.path.join(H.REPO, "dist"), BUILT_WHEELS):
        if has_wheel(candidate):
            return candidate
    os.makedirs(BUILT_WHEELS, exist_ok=True)
    problems = []
    for isolation in (["--no-build-isolation"], []):
        build = subprocess.run(
            [H.PYTHON, "-m", "pip", "wheel", "--no-deps", *isolation,
             "-w", BUILT_WHEELS, H.REPO],
            capture_output=True, text=True)
        if has_wheel(BUILT_WHEELS):
            return BUILT_WHEELS
        problems.append((build.stderr or build.stdout)[-300:])
    raise RuntimeError("could not build a selkies wheel to package: "
                       + " | ".join(problems))


def main() -> bool:
    res = H.Results("packaging-sim")
    try:
        wheels = wheel_dir()
    except RuntimeError as e:
        res.check("a selkies wheel is available to package", False, str(e))
        return res.summary()
    res.check("a selkies wheel is available to package", True, wheels)

    # The packaging scripts build a venv with the python3 they find on PATH. A
    # distro python3 without ensurepip cannot, and the resulting failure says
    # nothing about the scripts, so they are handed the interpreter the tests
    # already run on -- by name only, leaving the rest of PATH alone.
    shim = os.path.join(H.WORKDIR, "packaging-python")
    os.makedirs(shim, exist_ok=True)
    link = os.path.join(shim, "python3")
    if os.path.realpath(link) != os.path.realpath(H.PYTHON):
        if os.path.lexists(link):
            os.unlink(link)
        os.symlink(H.PYTHON, link)
    env = dict(os.environ, PATH=shim + os.pathsep + os.environ.get("PATH", ""))

    run = subprocess.run(["bash", SIMULATE, wheels], capture_output=True,
                         text=True, env=env)
    print(run.stdout, end="", flush=True)

    # One line per packager, in the table simulate.sh prints. Reported per
    # packager so a failure names the one that broke, with the script's own exit
    # status as the authority underneath.
    reported = set()
    for line in run.stdout.splitlines():
        fields = dict(re.findall(r"([a-z-]+)=(\S+)", line))
        name = line.split(" ", 1)[0]
        if name not in ("deb", "rpm", "apk", "arch") or "exit" not in fields:
            continue
        reported.add(name)
        res.check(f"{name} stages cleanly",
                  fields.get("exit") == "0"
                  and fields.get("repo-writes") == "none"
                  and fields.get("commands") == "linked",
                  line.strip()[:150])
    res.check("every packager was exercised",
              reported == {"deb", "rpm", "apk", "arch"}, sorted(reported))
    res.check("the simulation reports success", run.returncode == 0,
              (run.stderr or "").strip()[-300:])
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
