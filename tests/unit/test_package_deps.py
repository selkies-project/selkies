#!/usr/bin/env python3
"""The packaged dependency lists say the same thing as the wheel's.

The AppImage installs Selkies with `pip install --no-deps` and resolves the
dependencies through conda instead, so its recipe carries a second copy of
[project.dependencies]. A copy drifts: it kept PyAV long after the WebRTC
stack stopped using it, which put a GPL FFmpeg in the AppImage and made the
licensing page wrong. This holds the two lists to each other, name and floor,
and names the packages that legitimately come from pip because conda-forge has
none.
"""
import os
import re
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
PYPROJECT = os.path.join(REPO, "pyproject.toml")
RECIPE = os.path.join(REPO, "infra", "appimage", "recipe.yaml")
APPIMAGE = os.path.join(REPO, "scripts", "ci", "appimage.sh")

# conda-forge spells a few of them differently.
CONDA_NAME = {"msgpack": "msgpack-python", "pillow": "pillow"}
# No conda-forge package exists, so scripts/ci/appimage.sh pip-installs them
# into the same prefix.
PIP_ONLY = {"pixelflux", "pcmflux", "pulsectl-asyncio", "aitop"}

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [package-deps] {label}  {detail}", flush=True)


def wheel_dependencies() -> tuple:
    """[project.dependencies] as (required, legacy) name-to-floor maps.

    A requirement split across interpreters (aiohttp) keeps the floor of the
    branch for the newest Python, which is what the AppImage's own runtime is;
    one gated to Python below 3.10 is `legacy`, which the conda package may
    carry for the interpreters it also supports but need not.
    """
    body = open(PYPROJECT, encoding="utf-8").read()
    block = body.split("dependencies = [", 1)[1].split("]", 1)[0]
    required, legacy = {}, {}
    for line in block.splitlines():
        line = line.strip()
        if not line.startswith('"'):
            continue
        spec, _, marker = line.strip('",').partition(";")
        name = re.split(r"[<>=~!\[]", spec, maxsplit=1)[0].strip().lower()
        m = re.search(r">=\s*([0-9][0-9a-zA-Z.\-]*)", spec)
        (legacy if "python_version < " in marker else required)[name] = m.group(1) if m else ""
    return required, {n: f for n, f in legacy.items() if n not in required}


def recipe_run() -> dict:
    """Requirement name to floor from the recipe's `run:` list."""
    body = open(RECIPE, encoding="utf-8").read()
    block = body.split("  run:", 1)[1].split("\ntests:", 1)[0]
    out = {}
    for line in block.splitlines():
        line = line.strip()
        if not line.startswith("- "):
            continue
        item = line[2:].strip()
        name, _, rest = item.partition(" ")
        name = name.lower()
        if name == "python":
            continue
        m = re.search(r">=\s*([0-9][0-9a-zA-Z.\-]*)", rest)
        out[name] = m.group(1) if m else ""
    return out


wheel, legacy = wheel_dependencies()
recipe = recipe_run()
check("both lists parsed", bool(wheel) and bool(recipe),
      f"{len(wheel)} wheel, {len(recipe)} recipe")

expected = {CONDA_NAME.get(n, n): f for n, f in wheel.items() if n not in PIP_ONLY}
allowed = dict(expected, **{CONDA_NAME.get(n, n): f for n, f in legacy.items()})
missing = sorted(set(expected) - set(recipe))
extra = sorted(set(recipe) - set(allowed))
check("the recipe carries every conda-resolvable dependency", not missing,
      ", ".join(missing))
check("the recipe carries nothing the wheel does not depend on", not extra,
      ", ".join(extra))

mismatched = [f"{n}: wheel {allowed[n]!r} vs recipe {recipe[n]!r}"
              for n in sorted(set(allowed) & set(recipe))
              if allowed[n] != recipe[n]]
check("the floors agree", not mismatched, "; ".join(mismatched))

appimage = open(APPIMAGE, encoding="utf-8").read()
for name in sorted(PIP_ONLY):
    check(f"the AppImage pip-installs {name}", name in appimage, "scripts/ci/appimage.sh")

# Every medium builds the interposers from its own script, so one added to the
# packagers alone reaches no AppImage user, and one whose path is exported
# nowhere reaches no application at all.
for packager in ("deb.sh", "rpm.sh", "apk.sh", "arch.sh"):
    body = open(os.path.join(REPO, "infra", "packaging", packager), encoding="utf-8").read()
    for addon in ("interposer.sh", "v4l2-interposer.sh"):
        check(f"{packager} stages the {addon.rsplit('.', 1)[0]}",
              f"/{addon}" in body, f"infra/packaging/{packager}")

for source, env in (("js-interposer/joystick_interposer.c", "SELKIES_INTERPOSER"),
                    ("v4l2-interposer/v4l2_interposer.c", "SELKIES_WEBCAM_INTERPOSER")):
    check(f"the AppImage compiles {os.path.basename(source)}", source in appimage,
          "scripts/ci/appimage.sh")
    check(f"the AppImage exports {env}", f"export {env}=" in appimage,
          "scripts/ci/appimage.sh")

print(f"[package-deps] {passed}/{passed + failed} passed", flush=True)
sys.exit(1 if failed else 0)
