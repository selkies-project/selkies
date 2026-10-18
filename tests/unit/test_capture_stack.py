#!/usr/bin/env python3
"""Which pixelflux and pcmflux a build carries is decided by one shell step.

Images, packages, AppImages, and the suites install the capture stack from wheels
the run holds rather than from an index, and the `locate` step of
`build-pixelflux-pcmflux-wheels.yaml` is what names them: each project's
per-commit pre-release for a build ahead of a release, and the release carrying
the pinned version for a release, which is what lets a release be cut with
nothing of either project on PyPI. It reads the pins out of `pyproject.toml`, so
a pin spelled in a way it cannot read, or a release whose assets are of some
other version, is a build that quietly ships a capture stack the wheel did not
ask for. Ahead of a release it waits for a pre-release its project's CI is still
building rather than building the same wheels a second time, and builds what no
pre-release will cover.
"""
import json
import os
import re
import subprocess
import sys
import tempfile
import time

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
    # Every value reaches it as an environment variable
    return "\n".join(body)


try:
    STEP = probe_step()
except StopIteration:
    STEP = ""
check("the workflow still has the locate step this runs", bool(STEP.strip()) and "${{" not in STEP)
if not STEP.strip() or "${{" in STEP:
    print(f"[capture-stack] {passed}/{passed + failed} passed")
    sys.exit(1)

# `release list` answers STUB_TAGS from its STUB_TAGS_AFTER-th call on (the
# pre-release showing up while the step waits), `run list` the statuses of the
# commit's pre-release runs, and the commit's date is STUB_PUSHED.
STUB_GH = """#!/bin/sh
case "$1" in
  api)
    case "$2" in
      */commits/main) printf '%s\\n' "${STUB_SHA}" ;;
      *) printf '%s\\n' "${STUB_PUSHED}" ;;
    esac ;;
  release)
    case "$2" in
      list)
        n=$(( $(cat "${STUB_COUNT}" 2>/dev/null || echo 0) + 1 )); echo "$n" > "${STUB_COUNT}"
        [ "$n" -ge "${STUB_TAGS_AFTER:-1}" ] && printf '%s\\n' ${STUB_TAGS} ;;
      view) printf '%s\\n' ${STUB_ASSETS} ;;
    esac ;;
  run) printf '%s\\n' "${STUB_RUNS}" ;;
esac
"""


def probe(pinned: bool, pins=None, **stub) -> tuple:
    """The locate step, run against a stubbed `gh`: `(accepted, step outputs)`.

    `pins` writes a pyproject.toml carrying them; without it a pinned run reads
    the repository's own, which is what a release resolves. A HEAD run reads no
    pins and runs in a checkout of its own.
    """
    with tempfile.TemporaryDirectory() as tmp:
        os.mkdir(os.path.join(tmp, "bin"))
        gh = os.path.join(tmp, "bin", "gh")
        open(gh, "w").write(STUB_GH)
        os.chmod(gh, 0o755)
        if pins is None and pinned:
            cwd = REPO
        else:
            cwd = tmp
            deps = "".join(f'    "{pin}",\n' for pin in pins or [])
            open(os.path.join(tmp, "pyproject.toml"), "w").write(
                f'[project]\ndependencies = [\n    "Pillow",\n{deps}]\n')
        out = os.path.join(tmp, "github_output")
        open(out, "w").close()
        env = dict(os.environ, GITHUB_OUTPUT=out, PATH=os.path.join(tmp, "bin") + os.pathsep + os.environ["PATH"],
                   OWNER=OWNER, PINNED="true" if pinned else "false",
                   WAIT_S=str(stub.get("wait", 0)), SHOW_UP_S="600", POLL_S="0",
                   STUB_SHA=stub.get("sha", ""), STUB_TAGS=stub.get("tags", ""),
                   STUB_TAGS_AFTER=str(stub.get("tags_after", 1)), STUB_COUNT=os.path.join(tmp, "count"),
                   STUB_RUNS=stub.get("runs", ""), STUB_PUSHED=stub.get("pushed", "2026-01-01T00:00:00Z"),
                   STUB_ASSETS=stub.get("assets", ""))
        proc = subprocess.run(["bash", "-e", "-c", STEP], cwd=cwd, capture_output=True,
                              text=True, timeout=120, env=env)
        outputs = dict(ln.split("=", 1) for ln in open(out).read().splitlines() if "=" in ln)
    return proc.returncode == 0, outputs


PINS = dict(re.findall(r'"(pixelflux|pcmflux)[~=<>!]*([0-9][^"]*)"',
                       open(os.path.join(REPO, "pyproject.toml")).read()))
check("both capture-stack pins are in pyproject.toml", set(PINS) == {"pixelflux", "pcmflux"}, str(PINS))

def fetched(out: dict, project: str) -> int:
    """How many platform jobs the fetch matrix holds for `project`."""
    return sum(1 for entry in json.loads(out.get("fetch", "[]")) if entry["project"] == project)


# The pins this repository carries, against a release that has their wheels
assets = " ".join(f"{p}-{v}-cp312-cp312-manylinux_2_28_x86_64.whl" for p, v in PINS.items())
ok, out = probe(pinned=True, assets=assets)
check("a release run reads both pins", ok)
for project, version in PINS.items():
    check(f"{project} {version} is taken from the release of that version",
          out.get(project) == version, out.get(project, "<unset>"))
    check(f"{project} would be built as its release is, at its tag",
          out.get(f"{project}_version") == version and out.get(f"{project}_ref") == "",
          f"{out.get(f'{project}_version', '<unset>')} {out.get(f'{project}_ref', '<unset>')!r}")
    check(f"{project}'s four platforms are fetched", fetched(out, project) == 4, out.get("fetch", "<unset>"))

# A tag that moved: the release exists, its wheels are of another version
_, out = probe(pinned=True, assets=" ".join(f"{p}-0.0.1-cp312-cp312-manylinux_2_28_x86_64.whl"
                                            for p in PINS))
check("a release carrying another version's wheels is built instead",
      out.get("pixelflux") == "" and fetched(out, "pixelflux") == 0, out.get("fetch", "<unset>"))

# Nothing released for the pin at all
_, out = probe(pinned=True)
check("a pin no release carries is built from its tag",
      out.get("pixelflux") == "" and out.get("pixelflux_version") == PINS["pixelflux"],
      out.get("pixelflux_version", "<unset>"))

# pin -> the version the step reads out of it
SPELLINGS = {"pixelflux~=2.1.0": "2.1.0", "pixelflux==2.1.0rc1": "2.1.0rc1",
             "pixelflux>=2.1.0": "2.1.0", "pixelflux~=2.1.0.dev1": "2.1.0.dev1"}
for pin, version in SPELLINGS.items():
    _, out = probe(pinned=True, pins=[pin, "pcmflux~=2.1.0"])
    check(f"{pin} names {version}", out.get("pixelflux_version") == version,
          out.get("pixelflux_version", "<unset>"))
ok, _ = probe(pinned=True, pins=["pcmflux~=2.1.0"])
check("a missing pin fails the run rather than guessing one", not ok)

# Ahead of a release: the per-commit pre-release of each project's main HEAD,
# which is tagged with an abbreviated SHA
SHA = "a3290fdc0ffee0000000000000000000000000de"
_, out = probe(pinned=False, sha=SHA, tags="97ef61d a3290fd 57995fb")
check("a HEAD run takes the pre-release of the commit main is on",
      out.get("pixelflux") == "a3290fd" and fetched(out, "pixelflux") == 4, out.get("pixelflux", "<unset>"))
check("a HEAD run would build that commit, not whatever main is by then", out.get("pixelflux_ref") == SHA,
      out.get("pixelflux_ref", "<unset>"))
_, out = probe(pinned=False, sha=SHA, tags="97ef61d a3290fd", tags_after=3, runs="in_progress", wait=60)
check("a pre-release still building is waited for and taken",
      out.get("pixelflux") == "a3290fd", out.get("pixelflux", "<unset>"))
_, out = probe(pinned=False, sha=SHA, tags="a3290fd", tags_after=99, runs="queued", wait=0)
check("a pre-release that outlasts the wait is built here", out.get("pixelflux") == "",
      out.get("pixelflux", "<unset>"))
_, out = probe(pinned=False, sha=SHA, tags="97ef61d 57995fb", runs="completed", wait=60)
check("a HEAD whose pre-release run ended without one is built at once",
      out.get("pixelflux") == "" and fetched(out, "pixelflux") == 0, out.get("pixelflux", "<unset>"))
_, out = probe(pinned=False, sha=SHA, tags="97ef61d 57995fb", wait=60)
check("a HEAD long past its push with no pre-release run is built at once",
      out.get("pixelflux") == "", out.get("pixelflux", "<unset>"))
fresh = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
_, out = probe(pinned=False, sha=SHA, tags="a3290fd", tags_after=3, pushed=fresh, wait=60)
check("a HEAD pushed moments ago is waited for until its pre-release shows up",
      out.get("pixelflux") == "a3290fd", out.get("pixelflux", "<unset>"))
_, out = probe(pinned=False, tags="97ef61d a3290fd")
check("a HEAD the probe could not read is built from main",
      out.get("pixelflux") == "" and out.get("pixelflux_ref") == "main", out.get("pixelflux_ref", "<unset>"))

print(f"[capture-stack] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
