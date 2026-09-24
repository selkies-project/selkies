#!/usr/bin/env python3
"""The input interposer's hooks survive a signal handler that calls them.

The interposer hooks read, ioctl, close, and the stat family for every fd in
a process, and looks each fd up in its handle tables under a lock. A signal
handler that reads a pipe (uvloop's, an SDL game's) runs on whichever thread
the signal lands on, which may be one inside that lookup, so the lookup has
to be re-enterable from the handler: an fd hook takes no lock while no fake
device is open, and the table locks are recursive once one is.

Builds the interposer off the tree, then runs ``tests/tools/interposer_signal_stress.c``
under its preload: worker threads hammer the hooked calls while a timer
signal, steered onto them, reads a pipe from its handler. The process has to
exit within the budget; a hook the handler cannot re-enter deadlocks it.
"""
import os
import shutil
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, TESTS)
import helpers as H  # noqa: E402

res = H.Results("input-interposer-signals")
INTERPOSER = os.path.join(REPO, "addons", "input-interposer", "input_interposer.c")
STRESS = os.path.join(TESTS, "tools", "interposer_signal_stress.c")


def build(scratch: str) -> tuple:
    """The interposer and the stress tool, built off the tree."""
    lib = os.path.join(scratch, "selkies_input_interposer.so")
    subprocess.run(["gcc", "-shared", "-fPIC", "-o", lib, INTERPOSER, "-ldl", "-pthread"],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    tool = os.path.join(scratch, "interposer_signal_stress")
    subprocess.run(["gcc", "-O1", "-o", tool, STRESS, "-pthread"],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return lib, tool


def storm(lib: str, tool: str, scratch: str) -> tuple:
    """Run the stress tool under the preload, bounded well above its own three seconds.

    Returns:
        ``(exited, returncode)``; ``exited`` is False when the budget ran out.
    """
    env = dict(os.environ, LD_PRELOAD=lib, SELKIES_JS_SOCKET_PATH=scratch)
    try:
        done = subprocess.run([tool], env=env, timeout=20, capture_output=True, text=True)
    except subprocess.TimeoutExpired:
        return False, None
    return True, done.returncode


if shutil.which("gcc") is None:
    res.skip("the hooks survive a signal handler that calls them", "no gcc")
    sys.exit(0 if res.summary() else 1)

with tempfile.TemporaryDirectory() as scratch:
    lib, tool = build(scratch)
    for attempt in range(3):
        exited, code = storm(lib, tool, scratch)
        res.check(f"the hooks survive a signal handler that calls them (run {attempt + 1})",
                  exited and code == 0,
                  "deadlocked" if not exited else f"exit {code}")

sys.exit(0 if res.summary() else 1)
