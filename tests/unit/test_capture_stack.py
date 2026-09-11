#!/usr/bin/env python3
"""Which pixelflux and pcmflux a build carries is decided by one shell step.

Images, packages, AppImages and the suites install the capture stack from wheels
the run holds rather than from an index, and the `locate` step of
`build-pixelflux-pcmflux-wheels.yaml` is what names them: each project's
per-commit pre-release for a build ahead of a release, and the release carrying
the pinned version for a release, which is what lets a release be cut with
nothing of either project on PyPI. It reads the pins out of `pyproject.toml`, so
a pin spelled in a way it cannot read, or a release whose assets are of some
other version, is a build that quietly ships a capture stack the wheel did not
ask for.
"""
import os
import re
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
WORKFLOW = os.path.join(REPO, ".github", "workflows", "build-pixelflux-pcmflux-wheels.yaml")
OWNER = "selkies-project"

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [capture-stack] {label}  {detail}", flush=True)


def probe_step() -> str:
    """The script the workflow's locate step runs, lifted out of the YAML."""
    lines = open(WORKFLOW).read().splitlines()
    start = next(i for i, ln in enumerate(lines) if ln.strip() == "- id: probe")
    run = next(i for i in range(start, len(lines)) if lines[i].strip() == "run: |")
    indent = len(lines[run + 1]) - len(lines[run + 1].lstrip())
    body = []
    for ln in lines[run + 1:]:
        if ln.strip() and len(ln) - len(ln.lstrip()) < indent:
            break
        body.append(ln[indent:])
    # The one expression the step carries; every other value reaches it as an
    # environment variable
    return "\n".join(body).replace("${{ github.repository_owner }}", OWNER)


try:
    STEP = probe_step()
except StopIteration:
    STEP = ""
check("the workflow still has the locate step this runs", bool(STEP.strip()) and "${{" not in STEP)
if not STEP.strip() or "${{" in STEP:
    print(f"[capture-stack] {passed}/{passed + failed} passed")
    sys.exit(1)

STUB_GH = """#!/bin/sh
case "$1" in
  api) printf '%s\\n' "${STUB_SHA}" ;;
  release)
    case "$2" in
      list) printf '%s\\n' ${STUB_TAGS} ;;
      view) printf '%s\\n' ${STUB_ASSETS} ;;
    esac ;;
esac
"""


def probe(pinned: bool, pins=None, **stub) -> tuple:
    """The locate step, run against a stubbed `gh`: `(accepted, step outputs)`.

    `pins` writes a pyproject.toml carrying them; without it the repository's own
    is used, which is what a release resolves.
    """
    with tempfile.TemporaryDirectory() as tmp:
        os.mkdir(os.path.join(tmp, "bin"))
        gh = os.path.join(tmp, "bin", "gh")
        open(gh, "w").write(STUB_GH)
        os.chmod(gh, 0o755)
        if pins is None:
            cwd = REPO
        else:
            cwd = tmp
            deps = "".join(f'    "{pin}",\n' for pin in pins)
            open(os.path.join(tmp, "pyproject.toml"), "w").write(
                f'[project]\ndependencies = [\n    "Pillow",\n{deps}]\n')
        out = os.path.join(tmp, "github_output")
        open(out, "w").close()
        env = dict(os.environ, GITHUB_OUTPUT=out, PATH=os.path.join(tmp, "bin") + os.pathsep + os.environ["PATH"],
                   PINNED="true" if pinned else "false",
                   STUB_SHA=stub.get("sha", ""), STUB_TAGS=stub.get("tags", ""),
                   STUB_ASSETS=stub.get("assets", ""))
        proc = subprocess.run(["bash", "-e", "-c", STEP], cwd=cwd, capture_output=True,
                              text=True, timeout=120, env=env)
        outputs = dict(ln.split("=", 1) for ln in open(out).read().splitlines() if "=" in ln)
    return proc.returncode == 0, outputs


PINS = dict(re.findall(r'"(pixelflux|pcmflux)[~=<>!]*([0-9][^"]*)"',
                       open(os.path.join(REPO, "pyproject.toml")).read()))
check("both capture-stack pins are in pyproject.toml", set(PINS) == {"pixelflux", "pcmflux"}, str(PINS))

# The pins this repository carries, against a release that has their wheels
assets = " ".join(f"{p}-{v}-cp312-cp312-manylinux_2_28_x86_64.whl" for p, v in PINS.items())
ok, out = probe(pinned=True, assets=assets)
check("a release run reads both pins", ok)
for project, version in PINS.items():
    check(f"{project} {version} is taken from the release of that version",
          out.get(project) == version, out.get(project, "<unset>"))
    check(f"{project} would be built from its tag", out.get(f"{project}_ref") == f"refs/tags/{version}",
          out.get(f"{project}_ref", "<unset>"))
check("a released pin is fetched rather than built", out.get("build") == "false",
      out.get("build", "<unset>"))

# A tag that moved: the release exists, its wheels are of another version
_, out = probe(pinned=True, assets=" ".join(f"{p}-0.0.1-cp312-cp312-manylinux_2_28_x86_64.whl"
                                            for p in PINS))
check("a release carrying another version's wheels is built instead",
      out.get("build") == "true", out.get("build", "<unset>"))

# Nothing released for the pin at all
_, out = probe(pinned=True)
check("a pin no release carries is built from its tag", out.get("build") == "true",
      out.get("build", "<unset>"))

# pin -> the version the step reads out of it
SPELLINGS = {"pixelflux~=2.1.0": "2.1.0", "pixelflux==2.2.0rc1": "2.2.0rc1",
             "pixelflux>=2.1.0": "2.1.0", "pixelflux~=2.2.0.dev1": "2.2.0.dev1"}
for pin, version in SPELLINGS.items():
    _, out = probe(pinned=True, pins=[pin, "pcmflux~=2.1.0"])
    check(f"{pin} names {version}", out.get("pixelflux_ref") == f"refs/tags/{version}",
          out.get("pixelflux_ref", "<unset>"))
ok, _ = probe(pinned=True, pins=["pcmflux~=2.1.0"])
check("a missing pin fails the run rather than guessing one", not ok)

# Ahead of a release: the per-commit pre-release of each project's main HEAD,
# which is tagged with an abbreviated SHA
SHA = "a3290fdc0ffee0000000000000000000000000de"
_, out = probe(pinned=False, sha=SHA, tags="97ef61d a3290fd 57995fb")
check("a HEAD run takes the pre-release of the commit main is on",
      out.get("pixelflux") == "a3290fd", out.get("pixelflux", "<unset>"))
check("a HEAD run would build main", out.get("pixelflux_ref") == "main",
      out.get("pixelflux_ref", "<unset>"))
check("a HEAD covered by a pre-release is not built", out.get("build") == "false",
      out.get("build", "<unset>"))
_, out = probe(pinned=False, sha=SHA, tags="97ef61d 57995fb")
check("a HEAD no pre-release covers is built", out.get("build") == "true",
      out.get("build", "<unset>"))
_, out = probe(pinned=False, tags="97ef61d a3290fd")
check("a HEAD the probe could not read is built", out.get("build") == "true",
      out.get("build", "<unset>"))

print(f"[capture-stack] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
