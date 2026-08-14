#!/usr/bin/env python3
"""The example container's shell stays parseable and lint-clean.

These scripts run only inside the container against a live compositor and X
server, so CI cannot exercise their behaviour. It can still guard the failure
that reaches a user as a session that will not paint — or, for the Dockerfile,
as a build that dies minutes in: a quoting or syntax slip. Every service script
is parsed with `bash -n`, so is the shell inside each Dockerfile `RUN`, and —
where shellcheck is installed — the scripts are checked for shellcheck errors
(its warning level is style, and these predate a clean pass at that level, so
gating on it here would fail on debt this suite did not introduce).
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
    + glob.glob(os.path.join(EXAMPLE, "services", "*", "*.sh"))
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

# A RUN's shell is what the build actually executes: Docker drops the comment
# lines inside one and joins the continuations into a single command.
def run_bodies(dockerfile):
    bodies, current = [], None
    for line in open(dockerfile).read().splitlines():
        if line.strip().startswith("#"):
            continue
        if current is None:
            if line.startswith("RUN "):
                current = [line[4:]]
            else:
                continue
        else:
            current.append(line)
        if not current[-1].rstrip().endswith("\\"):
            bodies.append("\n".join(current).replace("\\" + "\n", " "))
            current = None
    return bodies


for dockerfile in (os.path.join(EXAMPLE, "Dockerfile"), os.path.join(REPO, "Dockerfile")):
    rel = os.path.relpath(dockerfile, REPO)
    bodies = run_bodies(dockerfile)
    check(f"{rel} has RUN blocks", bool(bodies), str(len(bodies)))
    for i, body in enumerate(bodies, 1):
        r = subprocess.run([bash, "-n"], input=body, capture_output=True, text=True)
        check(f"parse {rel} RUN #{i}", r.returncode == 0, r.stderr.strip()[:200])

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
