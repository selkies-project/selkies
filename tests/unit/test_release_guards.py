#!/usr/bin/env python3
"""Exercise release job gates and publication order without running jobs.

The GitHub expression subset used by these gates is evaluated against branch,
tag, caller, and input contexts. The checks need no Actions runner or Docker.

Usage: python3 tests/unit/test_release_guards.py
"""
import os
import re
import sys
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
WORKFLOWS = os.path.join(REPO, ".github", "workflows")

passed = failed = 0


def check(label: str, ok: bool, detail: str = "") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [release-guards] {label}  {detail}", flush=True)


def workflow(name: str) -> str:
    with open(os.path.join(WORKFLOWS, name)) as f:
        return f.read()


def job(source: str, name: str) -> str:
    match = re.search(rf"^  {re.escape(name)}:\n(.*?)(?=^  [a-z0-9-]+:\n|\Z)", source, re.M | re.S)
    if not match:
        raise AssertionError(f"workflow has no {name} job")
    return match.group(1)


def gate(source: str, name: str) -> str:
    match = re.search(r"^    if: \$\{\{ (.*?) \}\}$", job(source, name), re.M)
    if not match:
        raise AssertionError(f"{name} has no one-line job gate")
    return match.group(1)


def runs(expression: str, event: str, ref: str, tag_prefix: str) -> bool:
    """Evaluate the expressions' comparisons and boolean operators in an event context."""
    ref_name = ref.removeprefix("refs/heads/").removeprefix("refs/tags/")
    github = SimpleNamespace(event_name=event, ref=ref, ref_name=ref_name)
    inputs = SimpleNamespace(tag_prefix=tag_prefix)
    expression = expression.replace("&&", "and").replace("||", "or")
    return bool(eval(expression, {  # noqa: S307 - fixed workflow expressions, no globals or builtins
        "__builtins__": {},
        "startsWith": lambda value, prefix: value.startswith(prefix),
    }, {"github": github, "inputs": inputs}))


images = workflow("images.yaml")
for name in ("build", "addons"):
    expression = gate(images, name)
    cases = (
        ("main CI call", "workflow_call", "refs/heads/main", "main", True),
        ("main dispatch", "workflow_dispatch", "refs/heads/main", "main", True),
        ("manual release from main", "workflow_call", "refs/heads/main", "2.0.0rc0", True),
        ("pushed-tag release", "workflow_call", "refs/tags/2.0.0rc0", "2.0.0rc0", True),
        ("standalone tag dispatch", "workflow_dispatch", "refs/tags/2.0.0rc0", "2.0.0rc0", False),
        ("tag call with an empty prefix", "workflow_call", "refs/tags/2.0.0rc0", "", False),
        ("tag call with a mismatched prefix", "workflow_call", "refs/tags/2.0.0rc0", "latest", False),
        ("non-main branch call", "workflow_call", "refs/heads/topic", "main", False),
    )
    for label, event, ref, prefix, expected in cases:
        check(f"{name}: {label}", runs(expression, event, ref, prefix) == expected,
              f"event={event} ref={ref} tag_prefix={prefix!r}")
check("video and addon jobs use the same release gate", gate(images, "build") == gate(images, "addons"))
cleanup = gate(images, "cleanup").replace(" && !cancelled()", "")
check("tag releases leave registry cleanup alone", not runs(cleanup, "workflow_call", "refs/tags/2.0.0rc0", "2.0.0rc0"))
check("main builds retain registry cleanup", runs(cleanup, "workflow_call", "refs/heads/main", "main"))

published = workflow("release-published.yaml")
needs = {}
for name in ("pypi-pcmflux", "pypi-pixelflux", "pypi"):
    match = re.search(r"^    needs: (.*)$", job(published, name), re.M)
    needs[name] = [] if not match else [item.strip() for item in match.group(1).strip("[]").split(",")]
check("PCM wheels publish first", needs["pypi-pixelflux"] == ["pypi-pcmflux"], str(needs))
check("Selkies publishes after PCM and PixelFlux", needs["pypi"] == ["pypi-pixelflux"], str(needs))

ci = workflow("ci.yaml")
native = job(ci, "native-tests")
step = re.search(r"^      - name: Test the non-GPL pixelflux configuration\n(.*?)(?=^      - |\Z)", native, re.M | re.S)
condition_match = re.search(r"^        if: (.*)$", step.group(1), re.M) if step else None
condition = condition_match.group(1) if condition_match else "missing"
for arch in ("x86_64", "aarch64"):
    selected = step is not None and (condition == "missing" or bool(eval(
        condition.replace("&&", "and").replace("||", "or"),
        {"__builtins__": {}}, {"matrix": SimpleNamespace(arch=arch)})))
    check(f"non-GPL tests run on {arch}", selected)

print(f"[release-guards] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
