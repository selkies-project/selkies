#!/usr/bin/env python3
"""The server and its optional Python modules use one local native source tree.

These checks guard the workspace path dependencies, local wheel package
folders, dependency-image recipes, and artifact names consumed by the suites.
They do not build the native libraries or contact an external repository.

Usage: python3 tests/unit/test_capture_stack.py
"""
import os
import re
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
WORKFLOW = os.path.join(REPO, ".github", "workflows", "build-pixelflux-pcmflux-wheels.yaml")

passed = failed = 0


def check(label: str, ok: bool, detail: str = "") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [capture-stack] {label}  {detail}", flush=True)


manifest = open(os.path.join(REPO, "Cargo.toml")).read()
workflow = open(WORKFLOW).read()
postcreate = open(os.path.join(REPO, ".devcontainer", "postcreate.sh")).read()

for project in ("pixelflux", "pcmflux"):
    dependency = re.search(rf'^{project} = \{{([^}}]+)\}}$', manifest, re.M)
    check(f"{project} is a local path dependency",
          dependency is not None and f'path = "crates/{project}"' in dependency.group(1)
          and " git = " not in dependency.group(1) and " rev = " not in dependency.group(1))
    check(f"{project} is an explicit workspace member",
          f'"crates/{project}"' in manifest.split("default-members", 1)[0])
    check(f"the workflow plans from crates/{project}",
          'cd "crates/${project}"' in workflow and "for project in pixelflux pcmflux" in workflow)
    check(f"{project} wheels upload from its local wheelhouse",
          "path: crates/${{ matrix.group.project }}/wheelhouse/*.whl" in workflow
          and "name: ${{ matrix.group.project }}-wheels-" in workflow)
    check(f"{project} wheels come from its local package directory",
          "package-dir: crates/${{ matrix.group.project }}" in workflow)
    check(f"development installs the local {project} wheel",
          f"./crates/{project}" in postcreate and "pip wheel --no-deps" in postcreate)

check("no external release location or polling remains",
      "locate:" not in workflow and "gh release" not in workflow and "gh run" not in workflow
      and "repository: ${{ github.repository_owner }}" not in workflow and "src/${project}" not in workflow)
check("the wheel image jobs execute only the planned recipe",
      "RECIPE: ${{ matrix.image.recipe }}" in workflow and "bash /recipe.sh" in workflow)
check("pixelflux keeps its dependency image and job split",
      "image=pixelflux-wheel-deps" in workflow and 'jobs="manylinux=2 musllinux=4"' in workflow)
check("pcmflux keeps its dependency image and job split",
      "image=pcmflux-wheel-deps" in workflow and 'jobs="manylinux=1 musllinux=1"' in workflow)
check("release builds stamp component and root locks",
      'crates/${PROJECT}/Cargo.lock' in workflow and "Cargo.lock" in workflow
      and "inputs.package_version" in workflow)

print(f"[capture-stack] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
