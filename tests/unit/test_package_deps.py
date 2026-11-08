#!/usr/bin/env python3
"""Every distribution channel ships the server the same way.

The base image, the native packages, and the AppImage each build the Rust
server on their own distribution and each declare what it needs there, so a
fact one of them states the others have to state the same way, or one channel
ships a server another would not. Held here: the libraries pixelflux and pcmflux
opens by name at run time (no ELF dependency names them, so the image build,
the packages' dependency resolution, and the pristine-install check each carry
the list), the toolchain the server is built with, and that every packager
builds the server through infra/packaging/server.sh, declares its dependencies
through runtime-deps.sh, and stages both interposers, which the AppImage
compiles and exports the paths of too.
"""
import os
import re
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
PACKAGING = os.path.join(REPO, "infra", "packaging")
DOCKERFILE = os.path.join(REPO, "addons", "base", "Dockerfile")
VERIFY_PACKAGE = os.path.join(REPO, "scripts", "ci", "verify-package.sh")
RUNTIME_DEPS = os.path.join(PACKAGING, "runtime-deps.sh")
SERVER = os.path.join(PACKAGING, "server.sh")
APPIMAGE_SERVER = os.path.join(REPO, "infra", "appimage", "build-server.sh")
APPIMAGE = os.path.join(REPO, "scripts", "ci", "appimage.sh")

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [package-deps] {label}  {detail}", flush=True)


def read(path: str) -> str:
    return open(path, encoding="utf-8").read()


def sonames(text: str) -> set:
    return set(re.findall(r"lib[A-Za-z0-9_+.-]+\.so\.[0-9]+", text))


def assigned(path: str, name: str) -> set:
    """The sonames of the shell assignment `name="..."` in `path`."""
    m = re.search(rf'^{name}="([^"]*)"', read(path), re.M)
    return sonames(m.group(1)) if m else set()


def image_libraries() -> set:
    """The sonames the base image's final check loops over."""
    body = read(DOCKERFILE)
    m = re.search(r"for name in (.*?); do", body, re.S)
    return sonames(m.group(1)) if m else set()


image = image_libraries()
resolved = assigned(RUNTIME_DEPS, "RUNTIME_LOADED")
verified = assigned(VERIFY_PACKAGE, "NEEDED")
check("the image build names runtime-loaded libraries", bool(image),
      f"{len(image)} in addons/base/Dockerfile")
check("the packages declare the packages behind the same ones", resolved == image,
      " ".join(sorted(resolved ^ image)))
check("a pristine install is checked for the same ones", verified == image,
      " ".join(sorted(verified ^ image)))

toolchain = re.search(r'^ARG RUST_TOOLCHAIN="([^"]+)"', read(DOCKERFILE), re.M)
packaged = re.search(r'RUST_TOOLCHAIN="\$\{RUST_TOOLCHAIN:-([^}]+)\}"', read(SERVER))
check("the packages build with the image's toolchain",
      bool(toolchain) and bool(packaged) and toolchain.group(1) == packaged.group(1),
      f"{toolchain and toolchain.group(1)} vs {packaged and packaged.group(1)}")

for packager in ("deb.sh", "rpm.sh", "apk.sh", "arch.sh"):
    body = read(os.path.join(PACKAGING, packager))
    for step in ("server.sh", "runtime-deps.sh", "interposer.sh", "v4l2-interposer.sh"):
        check(f"{packager} runs {step}", f"/infra/packaging/{step} " in body,
              f"infra/packaging/{packager}")
    check(f"{packager} carries no Python prefix",
          "/opt/selkies" not in body and "mkvenv" not in body, f"infra/packaging/{packager}")

appimage = read(APPIMAGE)
check("the AppImage builds the server as the packages do",
      "/repo/infra/packaging/server.sh " in read(APPIMAGE_SERVER), "infra/appimage/build-server.sh")
for source, env in (("input-interposer/input_interposer.c", "SELKIES_INTERPOSER"),
                    ("v4l2-interposer/v4l2_interposer.c", "SELKIES_WEBCAM_INTERPOSER")):
    check(f"the AppImage compiles {os.path.basename(source)}", source in appimage,
          "scripts/ci/appimage.sh")
    check(f"the AppImage exports {env}", f"export {env}=" in appimage, "scripts/ci/appimage.sh")

print(f"[package-deps] {passed}/{passed + failed} passed", flush=True)
sys.exit(1 if failed else 0)
