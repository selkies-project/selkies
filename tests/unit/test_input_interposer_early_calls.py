#!/usr/bin/env python3
"""The input interposer's hooks work before its constructor has run.

The loader runs the constructors of a program's own libraries before that of a
preloaded one, so an application whose library opens a file or adds an eventfd
to epoll in a static initializer calls the hooks while the interposer has not
resolved any real libc entry point yet. Each hook has to resolve its own then,
not fail the call.

Builds the interposer off the tree, then runs ``tests/tools/interposer_early_calls.c``
under its preload: its library makes the hooked calls from its constructor, and the
program exits non-zero when one of them failed.
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

res = H.Results("input-interposer-early-calls")
INTERPOSER = os.path.join(REPO, "addons", "input-interposer", "input_interposer.c")
EARLY = os.path.join(TESTS, "tools", "interposer_early_calls.c")


def build(scratch: str) -> tuple:
    """The interposer, the library that calls it from its constructor, and the program."""
    lib = os.path.join(scratch, "selkies_input_interposer.so")
    subprocess.run(["gcc", "-shared", "-fPIC", "-o", lib, INTERPOSER, "-ldl", "-pthread"],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    early_lib = os.path.join(scratch, "libearly_calls.so")
    subprocess.run(["gcc", "-shared", "-fPIC", "-DEARLY_CALLS_LIB", "-o", early_lib, EARLY],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    tool = os.path.join(scratch, "interposer_early_calls")
    subprocess.run(["gcc", "-o", tool, EARLY, "-L", scratch, "-l:libearly_calls.so",
                    "-Wl,-rpath," + scratch],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return lib, tool


if shutil.which("gcc") is None:
    res.skip("hooked calls made before the constructor succeed", "no gcc")
    sys.exit(0 if res.summary() else 1)

with tempfile.TemporaryDirectory() as scratch:
    lib, tool = build(scratch)
    plain = subprocess.run([tool], capture_output=True, text=True, timeout=20)
    res.check("the calls succeed without the preload", plain.returncode == 0, plain.stderr.strip())
    env = dict(os.environ, LD_PRELOAD=lib, SELKIES_JS_SOCKET_PATH=scratch)
    done = subprocess.run([tool], env=env, capture_output=True, text=True, timeout=20)
    res.check("hooked calls made before the constructor succeed", done.returncode == 0,
              done.stderr.strip().replace("\n", "; "))

sys.exit(0 if res.summary() else 1)
