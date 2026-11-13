#!/usr/bin/env python3
"""The bundle and the base image build the server with one Rust toolchain.

The base image builds its server in its own `server` stage, and every other
channel repackages the bundle, whose server infra/packaging/server.sh builds:
a toolchain bumped in one and not the other ships two servers compiled
differently. scripts/ci/server-build-deps.sh reads the image's too, for the
checks CI runs outside a build.
"""
import os
import re
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
DOCKERFILE = os.path.join(REPO, "addons", "base", "Dockerfile")
SERVER = os.path.join(REPO, "infra", "packaging", "server.sh")


def read(path: str) -> str:
    """The text of `path`."""
    with open(path, encoding="utf-8") as f:
        return f.read()


def main() -> int:
    """Compare the two pins."""
    image = re.search(r'^ARG RUST_TOOLCHAIN="([^"]+)"', read(DOCKERFILE), re.M)
    bundle = re.search(r'RUST_TOOLCHAIN="\$\{RUST_TOOLCHAIN:-([^}]+)\}"', read(SERVER))
    ok = bool(image) and bool(bundle) and image.group(1) == bundle.group(1)
    print(f"{'PASS' if ok else 'FAIL'}  [toolchain] the bundle builds with the image's toolchain  "
          f"{image and image.group(1)} vs {bundle and bundle.group(1)}", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
