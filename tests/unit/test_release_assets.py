#!/usr/bin/env python3
"""What a release carries is whatever every job of the run uploaded, minus a list.

The draft release is assembled from one download of every artifact in the run,
so each job's incidental output -- an image build record, the conda package an
AppImage is assembled from, the capture-stack wheels that belong to their own
projects' releases, the empty digest files a manifest merge passes between jobs
-- lands beside the deliverables unless the collect step drops it. This runs
that step over a tree holding one of each and reads back what a user would see
on the release page.

Drives the step lifted out of the workflow; no network, no runner.

Usage: python3 tests/unit/test_release_assets.py
"""
import os
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
WORKFLOW = os.path.join(REPO, ".github", "workflows", "release.yaml")

# One of each: what the jobs of a release run leave in the artifact tree.
DELIVERED = (
    ("the wheel", "selkies-2.0.0rc0-py3-none-any.whl"),
    ("the sdist", "selkies-2.0.0rc0.tar.gz"),
    ("the AppImage", "selkies-2.0.0rc0-x86_64.AppImage"),
    ("the deb", "selkies_2.0.0.rc0-1.ubuntu26.04_amd64.deb"),
    ("the rpm", "selkies-2.0.0.rc0-1.el9.x86_64.rpm"),
    ("the apk", "selkies-2.0.0_rc0-r0-x86_64.apk"),
    ("the pacman package", "selkies-2.0.0rc0-1-x86_64.pkg.tar.zst"),
)
WITHHELD = (
    ("an image build record", "selkies-project.selkies.8M3GBU.dockerbuild"),
    ("the AppImage's conda package", "selkies-2.0.0rc0-pyh4616a5c_0.conda"),
    ("a pixelflux wheel", "pixelflux-2.1.0rc0-cp39-abi3-manylinux_2_28_x86_64.whl"),
    ("a pcmflux wheel", "pcmflux-2.1.0rc0-cp39-abi3-musllinux_1_2_aarch64.whl"),
    ("an image-digest placeholder", "a" * 64),
)

passed = failed = 0


def check(label: str, ok, detail: str = "") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [release-assets] {label}  {detail}", flush=True)


def collect_step() -> str:
    """The script the workflow's collect step runs, lifted out of the YAML."""
    lines = open(WORKFLOW).read().splitlines()
    start = next(i for i, ln in enumerate(lines)
                 if ln.strip() == "- name: Collect release files")
    run = next(i for i in range(start, len(lines)) if lines[i].strip() == "run: |")
    indent = len(lines[run + 1]) - len(lines[run + 1].lstrip())
    body = []
    for ln in lines[run + 1:]:
        if ln.strip() and len(ln) - len(ln.lstrip()) < indent:
            break
        body.append(ln[indent:])
    return "\n".join(body)


def released(names) -> set:
    """The file names the collect step leaves for the release, given `names`."""
    with tempfile.TemporaryDirectory() as work:
        arts = os.path.join(work, "release-artifacts", "one-job")
        os.makedirs(arts)
        for name in names:
            open(os.path.join(arts, name), "w").close()
        proc = subprocess.run(["bash", "-c", STEP], cwd=work,
                              capture_output=True, text=True, timeout=60)
        if proc.returncode != 0:
            check("the collect step runs", False, proc.stderr[-200:])
            return set()
        return set(os.listdir(os.path.join(work, "release")))


try:
    STEP = collect_step()
except StopIteration:
    STEP = ""
check("the workflow still has the collect step this runs", bool(STEP.strip()))
if not STEP.strip():
    print(f"[release-assets] {passed}/{passed + failed} passed")
    sys.exit(1)

delivered = {name for _, name in DELIVERED}
on_release = released(delivered | {name for _, name in WITHHELD})
for label, name in DELIVERED:
    check(f"a release carries {label}", name in on_release, name)
for label, name in WITHHELD:
    check(f"a release withholds {label}", name not in on_release, name)
check("nothing else reaches the release", on_release == delivered,
      str(sorted(on_release - delivered)))

print(f"[release-assets] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
