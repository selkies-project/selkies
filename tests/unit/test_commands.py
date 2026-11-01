#!/usr/bin/env python3
"""`selkies <command>` runs every tool, and `selkies` is the one console script.

Every packaging reaches each tool through the `selkies` executable (the
`selkies-<command>` scripts went in 2.2.0), so each command must name a
function its module defines, see its own arguments under its own name, and
leave anything else to the server, whose --help lists the commands.
"""
import ast
import os
import re
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
SRC = os.path.join(REPO, "src")
sys.path.insert(0, SRC)

from selkies.commands import COMMANDS  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    ok = bool(ok)
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [commands] {label}  {detail}", flush=True)


def selkies(*args: str) -> subprocess.CompletedProcess:
    """`selkies` from this tree, as `python -m selkies` runs it."""
    env = dict(os.environ, PYTHONPATH=SRC + os.pathsep + os.environ.get("PYTHONPATH", ""))
    return subprocess.run([sys.executable, "-m", "selkies", *args], capture_output=True, text=True,
                          env=env, cwd=REPO, timeout=60)


def main() -> bool:
    text = open(os.path.join(REPO, "pyproject.toml"), encoding="utf-8").read()
    table = text.split("[project.scripts]", 1)[1].split("\n[", 1)[0]
    scripts = dict(re.findall(r'^([\w-]+) = "([\w.]+:\w+)"$', table, re.M))
    check("selkies is the one console script", scripts == {"selkies": "selkies.__main__:main"}, scripts)
    for name, (module, function, _) in COMMANDS.items():
        path = os.path.join(SRC, *module.split(".")) + ".py"
        tree = ast.parse(open(path, encoding="utf-8").read())
        defined = {n.name for n in tree.body if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))}
        check(f"selkies {name} runs a function {module} defines", function in defined, f"{module}:{function}")

    run = selkies("session", "--help")
    check("selkies session takes its own arguments, under its own name",
          run.returncode == 0 and run.stdout.startswith("usage: selkies session "), (run.returncode, run.stdout[:60]))
    run = selkies("resize")
    check("selkies resize sees the arguments after its name, none here",
          run.returncode == 2 and run.stderr.startswith("usage: selkies resize "), (run.returncode, run.stderr[:60]))
    run = selkies("resize", "--help")
    check("selkies resize answers --help rather than taking it for a size",
          run.returncode == 0 and run.stdout.startswith("usage: selkies resize "), (run.returncode, run.stdout[:60]))
    run = selkies("session", "--version")
    check("--version answers before any command parses it",
          run.returncode == 0 and run.stdout.startswith("selkies "), (run.returncode, run.stdout))
    run = selkies("--help")
    listed = [n for n in COMMANDS if re.search(rf"^  selkies {re.escape(n)} ", run.stdout, re.M)]
    check("the server's --help lists every command", run.returncode == 0 and listed == list(COMMANDS),
          (run.returncode, listed))

    print(f"[commands] {passed}/{passed + failed} passed", flush=True)
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
