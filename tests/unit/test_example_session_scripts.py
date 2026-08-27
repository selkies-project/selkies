#!/usr/bin/env python3
"""The shell the repository ships stays parseable and lint-clean.

These scripts run only inside the container against a live compositor and X
server, so CI cannot exercise their behaviour. It can still guard the failure
that reaches a user as a session that will not paint — or, for the Dockerfile,
as a build that dies minutes in: a quoting or syntax slip. Every service script
is parsed with `bash -n`, so is the shell inside each Dockerfile `RUN`, and —
where shellcheck is installed — every shell script the repository ships is
linted at shellcheck's lowest severity, which the tree passes.
"""
import glob
import re
import os
import shutil
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
# The session runtime lives in the base container; the example adds the
# desktop on top of it, so both directories carry scripts a session depends on.
BASE = os.path.join(REPO, "addons", "base")
EXAMPLE = os.path.join(REPO, "addons", "example")

SCRIPTS = sorted(
    p
    for root in (BASE, EXAMPLE)
    for p in (glob.glob(os.path.join(root, "*.sh"))
              + glob.glob(os.path.join(root, "services", "*", "run"))
              + glob.glob(os.path.join(root, "services", "*", "finish"))
              + glob.glob(os.path.join(root, "services", "*", "*.sh")))
)

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [example-shell] {label}  {detail}", flush=True)


bash = shutil.which("bash")
if not bash:
    print("SKIP bash not found, so the session scripts cannot be parsed", flush=True)
    sys.exit(77)

if not SCRIPTS:
    print("FAIL  [example-shell] scripts found  none matched under addons/base or addons/example", flush=True)
    sys.exit(1)

for path in SCRIPTS:
    rel = os.path.relpath(path, REPO)
    r = subprocess.run([bash, "-n", path], capture_output=True, text=True)
    check(f"parse {rel}", r.returncode == 0, r.stderr.strip()[:200])

def run_bodies(dockerfile: str) -> list:
    """The shell body of each RUN in `dockerfile`, as the build executes it:
    Docker drops the comment lines inside one and joins the continuations
    into a single command."""
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


def unquoted_settings(dockerfile: str) -> list:
    """`ARG`/`ENV` lines whose value is not double-quoted.

    Docker strips the quotes, so they change no value, but an unquoted one
    silently truncates at the first space and reads as a shell expansion where
    it is not one. Keeping every definition quoted makes both impossible to
    introduce by copy.
    """
    setting = re.compile(r'^\s*(?:ARG|ENV)\s+[A-Za-z_][A-Za-z0-9_]*=(.*)$')
    return [line for line in open(dockerfile).read().splitlines()
            if (m := setting.match(line)) and not m.group(1).startswith('"')]


DOCKERFILES = sorted(glob.glob(os.path.join(REPO, "**", "Dockerfile"), recursive=True))
DOCKERFILES = [p for p in DOCKERFILES if "node_modules" not in p]
check("Dockerfiles found", bool(DOCKERFILES), str(len(DOCKERFILES)))
for dockerfile in DOCKERFILES:
    rel = os.path.relpath(dockerfile, REPO)
    bodies = run_bodies(dockerfile)
    for i, body in enumerate(bodies, 1):
        r = subprocess.run([bash, "-n"], input=body, capture_output=True, text=True)
        check(f"parse {rel} RUN #{i}", r.returncode == 0, r.stderr.strip()[:200])
    loose = unquoted_settings(dockerfile)
    check(f"{rel} quotes every ARG/ENV value", not loose, "; ".join(loose)[:200])

PY_HELPERS = sorted(glob.glob(os.path.join(BASE, "services", "*", "*.py"))
                    + glob.glob(os.path.join(EXAMPLE, "services", "*", "*.py")))
for path in PY_HELPERS:
    rel = os.path.relpath(path, REPO)
    r = subprocess.run([sys.executable, "-m", "py_compile", path],
                       capture_output=True, text=True)
    check(f"compile {rel}", r.returncode == 0, r.stderr.strip()[:200])

# Two absences invisible until a desktop is in front of someone: no menu prefix
# is an empty application menu, and a raised latency must reach the daemons.
entrypoint = open(os.path.join(BASE, "container-entrypoint.sh")).read()
check("the shared environment carries the menu prefix",
      "XDG_MENU_PREFIX" in entrypoint,
      "container-entrypoint.sh")
for service in ("pipewire", "pipewire-pulse", "wireplumber"):
    path = os.path.join(BASE, "services", service, "run")
    body = open(path).read()
    check(f"{service} takes the audio latency an operator set",
          "${PIPEWIRE_LATENCY:-" in body, os.path.relpath(path, REPO))

# The interposers answer for /dev/video0 and /dev/input in the session's
# applications; preloaded into the backend they would hide the real device nodes
# capture and gamepads need, and their process-wide hooks block the asyncio loop.
# Run rather than grepped: what matters is the value the backend is exec'd with.
check("the session preloads both interposers",
      "SELKIES_WEBCAM_INTERPOSER" in entrypoint
      and all(v in entrypoint.split("LD_PRELOAD=")[1][:250]
              for v in ("${SELKIES_INTERPOSER}", "${SELKIES_WEBCAM_INTERPOSER}")),
      "container-entrypoint.sh")

with tempfile.TemporaryDirectory() as tmp:
    stub_bin = os.path.join(tmp, "bin")
    os.makedirs(stub_bin)
    with open(os.path.join(stub_bin, "selkies"), "w") as fh:
        fh.write('#!/bin/sh\nprintf %s "${LD_PRELOAD-unset}"\n')
    os.chmod(os.path.join(stub_bin, "selkies"), 0o755)
    shims = {
        "SELKIES_INTERPOSER": "/usr/$LIB/selkies_joystick_interposer.so",
        "FAKE_UDEV_LIB": "/usr/$LIB/libudev.so.1.0.0-fake",
        "SELKIES_WEBCAM_INTERPOSER": "/usr/$LIB/selkies_v4l2_interposer.so",
    }
    preload = ":".join([shims["SELKIES_INTERPOSER"], "/opt/operator.so",
                        shims["FAKE_UDEV_LIB"], shims["SELKIES_WEBCAM_INTERPOSER"]])
    r = subprocess.run(
        [bash, os.path.join(BASE, "selkies-entrypoint.sh")],
        capture_output=True, text=True,
        env={"PATH": stub_bin + os.pathsep + os.environ.get("PATH", ""),
             "XDG_RUNTIME_DIR": tmp,
             "SELKIES_WAYLAND": "true",
             "LD_PRELOAD": preload, **shims})
    check("the backend runs with no Selkies interposer preloaded",
          r.stdout == "/opt/operator.so", repr(r.stdout)[:200])

# Xft resources reach a toolkit only at start, so on X11 the DPI ladder's reload
# signal needs an XSETTINGS manager to signal, or running apps keep their density.
ladder = open(os.path.join(REPO, "src", "selkies", "display_utils.py")).read()
xsettingsd_service = os.path.join(BASE, "services", "xsettingsd", "run")
check("the DPI ladder's XSETTINGS manager is a service",
      "xsettingsd" not in ladder or os.path.isfile(xsettingsd_service),
      os.path.relpath(xsettingsd_service, REPO))
# The platform theme is what carries the session's fonts and icons into Qt, and
# it is the wrapper this service replaces that would otherwise export it.
lxqt_run = open(os.path.join(EXAMPLE, "services", "lxqt", "run")).read()
for var in ("QT_QPA_PLATFORMTHEME", "QT_AUTO_SCREEN_SCALE_FACTOR"):
    check(f"the session exports {var} on both backends",
          len(re.findall(rf'^export {var}=', lxqt_run, re.M)) == 1,
          "services/lxqt/run")

SKIP_DIRS = {".git", "node_modules", "dist", "__pycache__", ".venv", "venv"}


def shell_scripts(root: str) -> list:
    """Every shell script under `root`, by extension or by shebang.

    Walked rather than globbed: `glob` does not descend into dot-directories,
    which hides the devcontainer's scripts, and the scripts a container runs by
    name (service `run`/`finish`, `selkies-proot`) carry no extension at all.
    """
    found = []
    for base, dirs, files in os.walk(root):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            path = os.path.join(base, name)
            if name.endswith(".sh"):
                found.append(path)
                continue
            if os.path.splitext(name)[1] or os.path.islink(path):
                continue
            try:
                with open(path, "rb") as f:
                    shebang = f.readline(128)
            except OSError:
                continue
            if shebang.startswith(b"#!") and re.search(rb"\b(?:ba|da|k|z)?sh\b", shebang):
                found.append(path)
    return sorted(found)


# Every shell script the repository ships: packaging, CI and tooling scripts run
# unattended, where a quoting slip is the same failure with nobody watching.
ALL_SHELL = shell_scripts(REPO)

shellcheck = shutil.which("shellcheck")
if shellcheck:
    check("shell scripts found to lint", len(ALL_SHELL) >= len(SCRIPTS), str(len(ALL_SHELL)))
    for path in ALL_SHELL:
        rel = os.path.relpath(path, REPO)
        # The tree passes the lowest severity, so anything new is this run's. The
        # scripts' `shellcheck source=` directives are repo-relative, hence the path.
        r = subprocess.run([shellcheck, "-x", f"--source-path={REPO}",
                            "--severity=style", path],
                           capture_output=True, text=True)
        check(f"lint {rel}", r.returncode == 0, r.stdout.strip()[:400])
else:
    print("SKIP shellcheck not installed; parsed with bash -n only", flush=True)

print(f"[example-shell] {passed}/{passed + failed} passed", flush=True)
sys.exit(1 if failed else 0)
