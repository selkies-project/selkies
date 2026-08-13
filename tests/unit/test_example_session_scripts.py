#!/usr/bin/env python3
"""The example container's session shell scripts stay parseable and lint-clean.

These scripts run only inside the container against a live sway and X server, so
CI cannot exercise their behaviour. It can still guard the failure that reaches
a user as a session that will not paint: a quoting or syntax slip in the panel
keeper or a service `run`. Every script is parsed with `bash -n`, and — where
shellcheck is installed — checked for shellcheck errors (its warning level is
style, and the example scripts predate a clean pass at that level, so gating on
it here would fail on debt this suite did not introduce).
"""
import glob
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
EXAMPLE = os.path.join(REPO, "addons", "example")

SCRIPTS = sorted(
    glob.glob(os.path.join(EXAMPLE, "*.sh"))
    + glob.glob(os.path.join(EXAMPLE, "services", "*", "run"))
    + glob.glob(os.path.join(EXAMPLE, "services", "*", "finish"))
    + [os.path.join(EXAMPLE, "services", "lxqt", "anchor.sh")]
)

passed = failed = 0


def check(label, ok, detail=""):
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [example-shell] {label}  {detail}", flush=True)


bash = shutil.which("bash")
if not bash:
    print("SKIP bash not found, so the session scripts cannot be parsed", flush=True)
    sys.exit(77)

if not SCRIPTS:
    print("FAIL  [example-shell] scripts found  none matched under addons/example", flush=True)
    sys.exit(1)

for path in SCRIPTS:
    rel = os.path.relpath(path, REPO)
    r = subprocess.run([bash, "-n", path], capture_output=True, text=True)
    check(f"parse {rel}", r.returncode == 0, r.stderr.strip()[:200])

PY_HELPERS = sorted(glob.glob(os.path.join(EXAMPLE, "services", "*", "*.py")))
for path in PY_HELPERS:
    rel = os.path.relpath(path, REPO)
    r = subprocess.run([sys.executable, "-m", "py_compile", path],
                       capture_output=True, text=True)
    check(f"compile {rel}", r.returncode == 0, r.stderr.strip()[:200])

shellcheck = shutil.which("shellcheck")
if shellcheck:
    for path in SCRIPTS:
        rel = os.path.relpath(path, REPO)
        r = subprocess.run([shellcheck, "-x", "--severity=error", path],
                           capture_output=True, text=True)
        check(f"lint {rel}", r.returncode == 0, r.stdout.strip()[:400])
else:
    print("SKIP shellcheck not installed; parsed with bash -n only", flush=True)

print(f"[example-shell] {passed}/{passed + failed} passed", flush=True)
sys.exit(1 if failed else 0)
