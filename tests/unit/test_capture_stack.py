#!/usr/bin/env python3
"""Which pixelflux and pcmflux the suites drive is decided by one shell step.

The suites install the capture stack's Python modules from wheels the run holds
rather than from an index, and the `locate` step of
`build-pixelflux-pcmflux-wheels.yaml` is what names them: each project's
per-commit pre-release of the commit the workspace's `Cargo.toml` pins, which is
the commit the server under test links. It reads the pins out of `Cargo.toml`,
so a pin spelled in a way it cannot read is a run whose suites drive a capture
stack the server was not built against. It waits for a pre-release its
project's CI is still building rather than building the same wheels a second
time, and builds what no pre-release will cover. What it builds, the image jobs
prepare from the recipe the plan read out of each project: they check nothing
out and run that recipe in their container alone, so no code of another
repository's commit runs on a runner of the default branch.
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
# commit's pre-release runs until that call reaches STUB_DONE_AFTER and
# `completed` from then on (the run ending once its wheels are attached), and the
# commit's date is STUB_PUSHED.
STUB_GH = """#!/bin/sh
case "$1" in
  api) printf '%s\\n' "${STUB_PUSHED}" ;;
  release)
    n=$(( $(cat "${STUB_COUNT}" 2>/dev/null || echo 0) + 1 )); echo "$n" > "${STUB_COUNT}"
    [ "$n" -ge "${STUB_TAGS_AFTER:-1}" ] && printf '%s\\n' ${STUB_TAGS} ;;
  run)
    if [ "$(cat "${STUB_COUNT}" 2>/dev/null || echo 0)" -ge "${STUB_DONE_AFTER:-999999}" ]; then
      echo completed
    else
      printf '%s\\n' "${STUB_RUNS}"
    fi ;;
esac
"""

# The workspace's own pins, which is what every run resolves
PINS = dict(re.findall(r'^(pixelflux|pcmflux) = \{.* rev = "([0-9a-f]+)"',
                       open(os.path.join(REPO, "Cargo.toml")).read(), re.M))
check("both capture-stack pins are commits in Cargo.toml",
      set(PINS) == {"pixelflux", "pcmflux"} and all(len(sha) == 40 for sha in PINS.values()), str(PINS))


def manifest(pins: dict) -> str:
    """A workspace manifest carrying `pins`, spelled as the repository's is."""
    deps = "".join(f'{project} = {{ git = "https://github.com/{OWNER}/{project}", rev = "{sha}", '
                   f'default-features = false }}\n' for project, sha in pins.items())
    return f'[workspace]\nmembers = []\n\n[workspace.dependencies]\n{deps}'


def probe(pins=None, **stub) -> tuple:
    """The locate step, run against a stubbed `gh`: `(accepted, step outputs)`.

    `pins` writes a Cargo.toml carrying them; without it the step reads the
    repository's own.
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
            open(os.path.join(tmp, "Cargo.toml"), "w").write(manifest(pins))
        out = os.path.join(tmp, "github_output")
        open(out, "w").close()
        env = dict(os.environ, GITHUB_OUTPUT=out, PATH=os.path.join(tmp, "bin") + os.pathsep + os.environ["PATH"],
                   OWNER=OWNER, WAIT_S=str(stub.get("wait", 0)), SHOW_UP_S="600", POLL_S="0",
                   STUB_TAGS=stub.get("tags", ""), STUB_TAGS_AFTER=str(stub.get("tags_after", 1)),
                   STUB_COUNT=os.path.join(tmp, "count"), STUB_RUNS=stub.get("runs", ""),
                   STUB_DONE_AFTER=str(stub.get("done_after", 999999)),
                   STUB_PUSHED=stub.get("pushed", "2026-01-01T00:00:00Z"))
        proc = subprocess.run(["bash", "-e", "-c", STEP], cwd=cwd, capture_output=True,
                              text=True, timeout=120, env=env)
        outputs = dict(ln.split("=", 1) for ln in open(out).read().splitlines() if "=" in ln)
    return proc.returncode == 0, outputs


def fetched(out: dict, project: str) -> int:
    """How many platform jobs the fetch matrix holds for `project`."""
    return sum(1 for entry in json.loads(out.get("fetch", "[]")) if entry["project"] == project)


# The pins this repository carries, against releases tagged with their abbreviated SHAs
ok, out = probe(tags=" ".join(sha[:7] for sha in PINS.values()))
check("a run reads both of the repository's pins", ok)
for project, sha in PINS.items():
    check(f"{project} is taken from the pre-release of {sha[:7]}", out.get(project) == sha[:7],
          out.get(project, "<unset>"))
    check(f"{project} would be built at that commit", out.get(f"{project}_ref") == sha,
          out.get(f"{project}_ref", "<unset>"))
    check(f"{project}'s four platforms are fetched", fetched(out, project) == 4, out.get("fetch", "<unset>"))

ok, _ = probe(pins={"pcmflux": "0" * 40})
check("a missing pin fails the run rather than guessing one", not ok)

# Pre-releases are tagged with an abbreviated SHA
SHA = "a3290fdc0ffee0000000000000000000000000de"
OTHER = {"pcmflux": "57995fb" + "0" * 33}
_, out = probe(pins={"pixelflux": SHA, **OTHER}, tags="97ef61d a3290fd 57995fb")
check("the pre-release of the pinned commit is taken",
      out.get("pixelflux") == "a3290fd" and fetched(out, "pixelflux") == 4, out.get("pixelflux", "<unset>"))
check("a build checks out the pinned commit", out.get("pixelflux_ref") == SHA,
      out.get("pixelflux_ref", "<unset>"))
_, out = probe(pins={"pixelflux": SHA, **OTHER}, tags="97ef61d a3290fd", tags_after=3, runs="in_progress",
               done_after=5, wait=60)
check("a pre-release still building is waited for and taken",
      out.get("pixelflux") == "a3290fd", out.get("pixelflux", "<unset>"))
# The run tags the release before its wheels finish uploading, and one still
# uploading cannot be downloaded
_, out = probe(pins={"pixelflux": SHA, **OTHER}, tags="a3290fd", runs="in_progress", done_after=3, wait=60)
check("a pre-release tagged while its run still attaches the wheels is taken once the run ends",
      out.get("pixelflux") == "a3290fd", out.get("pixelflux", "<unset>"))
_, out = probe(pins={"pixelflux": SHA, **OTHER}, tags="a3290fd", runs="in_progress", wait=0)
check("nor taken while the run is in flight: the wait running out builds it here",
      out.get("pixelflux") == "", out.get("pixelflux", "<unset>"))
_, out = probe(pins={"pixelflux": SHA, **OTHER}, tags="a3290fd", tags_after=99, runs="queued", wait=0)
check("a pre-release that outlasts the wait is built here", out.get("pixelflux") == "",
      out.get("pixelflux", "<unset>"))
_, out = probe(pins={"pixelflux": SHA, **OTHER}, tags="97ef61d 57995fb", runs="completed", wait=60)
check("a commit whose pre-release run ended without one is built at once",
      out.get("pixelflux") == "" and fetched(out, "pixelflux") == 0, out.get("pixelflux", "<unset>"))
_, out = probe(pins={"pixelflux": SHA, **OTHER}, tags="97ef61d 57995fb", wait=60)
check("a commit long past its push with no pre-release run, such as one off main, is built at once",
      out.get("pixelflux") == "", out.get("pixelflux", "<unset>"))
fresh = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
_, out = probe(pins={"pixelflux": SHA, **OTHER}, tags="a3290fd", tags_after=3, pushed=fresh, wait=60)
check("a commit pushed moments ago is waited for until its pre-release shows up",
      out.get("pixelflux") == "a3290fd", out.get("pixelflux", "<unset>"))

text = open(WORKFLOW).read()
image_job = text[text.index("\n  image:\n"):text.index("\n  build:\n")]
check("the image jobs check nothing out and run the recipe the plan carries",
      "actions/checkout" not in image_job and "${{ matrix.image.recipe }}" in image_job
      and "recipe: $recipe" in text)

print(f"[capture-stack] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
