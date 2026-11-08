#!/usr/bin/env python3
"""Native abi3 and server wheels are selected by their parsed compatibility tags."""
import importlib.util
import os
import subprocess
import sys

from packaging.tags import Tag, sys_tags
from packaging.utils import parse_wheel_filename

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
SELECTOR = os.path.join(
    REPO, ".devcontainer", "features", "desktop-selkies", "src", "select-compatible-wheels.py"
)
spec = importlib.util.spec_from_file_location("select_compatible_wheels", SELECTOR)
selector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(selector)

passed = failed = 0


def check(label: str, ok: bool, detail: str = "") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [wheel-selection] {label}  {detail}", flush=True)


def tags(interpreter: str, abi: str, platform: str = "manylinux_2_28_x86_64"):
    return {Tag(interpreter, abi, platform), Tag("py3", "none", platform)}


P = "manylinux_2_28_x86_64"
ABI3 = "pixelflux-2.3.0-cp311-abi3-manylinux_2_28_x86_64.whl"
PCM = "pcmflux-2.2.2-cp311-abi3-manylinux_2_28_x86_64.whl"
ROOT = "selkies-2.3.0-py3-none-manylinux_2_28_x86_64.whl"
OLD = "pixelflux-2.2.3-cp312-cp312-manylinux_2_28_x86_64.whl"

for minor in range(11, 16):
    supported = {Tag(f"cp{minor}", f"cp{minor}", P), Tag("cp311", "abi3", P), Tag("py3", "none", P)}
    check(f"cp311-abi3 fits CPython 3.{minor}", selector.compatible("pixelflux", ABI3, supported))
    check(f"the server wheel fits CPython 3.{minor}", selector.compatible("selkies", ROOT, supported))
for minor in (9, 10):
    check(f"cp311-abi3 is refused on CPython 3.{minor}",
          not selector.compatible("pixelflux", ABI3, tags(f"cp{minor}", f"cp{minor}")))
    check(f"the server wheel still fits CPython 3.{minor}",
          selector.compatible("selkies", ROOT, tags(f"cp{minor}", f"cp{minor}")))
check("old full-API wheels fit their own interpreter", selector.compatible("pixelflux", OLD, tags("cp312", "cp312")))
check("old full-API wheels do not fit a newer interpreter", not selector.compatible(
    "pixelflux", OLD, {Tag("cp313", "cp313", P), Tag("cp311", "abi3", P)}))
check("abi3 is refused on free-threaded CPython",
      not selector.compatible("pixelflux", ABI3, tags("cp313", "cp313t")))
check("abi3 is refused for another project", not selector.compatible(
    "pcmflux", ABI3, {Tag("cp311", "abi3", P)}))
check("abi3 is refused for another platform", not selector.compatible(
    "pixelflux", ABI3, {Tag("cp311", "abi3", "manylinux_2_28_aarch64")}))
check("a malformed filename is not a wheel", not selector.compatible(
    "pixelflux", "pixelflux-2.3.0-abi3.tar.gz", {Tag("cp311", "abi3", P)}))
check("an unrelated abi3 asset is not accepted as pcmflux", not selector.compatible(
    "pcmflux", ABI3.replace("pixelflux", "some-other-project"), {Tag("cp311", "abi3", P)}))

platform = next(tag.platform for tag in sys_tags() if tag.platform != "any")
assets = (
    f"https://example.invalid/{ABI3} sha256:abc\n"
    f"https://example.invalid/not-a-wheel.txt sha256:def\n"
    f"https://example.invalid/{PCM} sha256:123\n"
)
selected = subprocess.run(
    [sys.executable, SELECTOR, "pcmflux"], input=assets, check=True, capture_output=True, text=True
).stdout.splitlines()
expected = ([f"https://example.invalid/{PCM} sha256:123"]
            if not parse_wheel_filename(PCM)[3].isdisjoint(sys_tags()) else [])
check("the CLI preserves only compatible release assets and digests", selected == expected,
      f"running platform {platform}")

workflow = open(os.path.join(REPO, ".github", "workflows", "tests.yaml"), encoding="utf-8").read()
check("the integration workflow uses the shared parser",
      'python "${SELECTOR}" "${project}"' in workflow and 'WHEEL_GLOB="*${ABI}-' not in workflow)

print(f"[wheel-selection] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
