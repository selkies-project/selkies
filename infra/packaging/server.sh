#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the Selkies server on this distribution and stage it, with the web
# client, under a package root: the binary at usr/bin/selkies and the bundle at
# usr/share/selkies/web beside it, where the server looks for its client.
#
# The build runs where the package is installed from, so the binary links the
# sonames this distribution ships, and the codec libraries it lacks are built
# by codecs.sh and linked in. The toolchain is the pinned release the images
# build with, installed by rustup-init checked against the hash rustup
# publishes beside it. pixelflux and pcmflux are git dependencies at the
# commits Cargo.lock pins, which Cargo fetches, so the build needs git and the
# network (or a `cargo vendor` tree configured for an offline one). The
# checkout stays read-only:
# the manifests are copied out so the release version can be stamped into
# them, and everything Cargo writes goes under /build.
#
# The web client is built once per run by scripts/ci/build-web.sh into
# dist/web, which every channel installs as it is.
#
# With --wheel, maturin builds the selkies wheel from the same copy instead
# (pyproject.toml, `bindings = "bin"`), with the release version as it is
# spelled in the wheel's metadata and the web client staged as its data at
# share/selkies/web; infra/wheel/build.sh repairs what it leaves in <out-dir>.
# SELKIES_SDIST=1 writes the sdist of that copy beside it.
#
# Usage: server.sh <pkg-root>          (SELKIES_VERSION stamps the version)
#        server.sh --wheel <out-dir>
set -eux
WHEEL_OUT=""
if [ "${1:-}" = --wheel ]; then
    WHEEL_OUT="${2:?usage: server.sh --wheel <out-dir>}"
else
    PKG_ROOT="${1:?usage: server.sh <pkg-root>}"
fi
REPO="$(dirname "$(readlink -f "$0")")/../.."
RUST_TOOLCHAIN="${RUST_TOOLCHAIN:-1.99.1}"
BUILD="${SELKIES_BUILD_DIR:-/build}"

test -f "${REPO}/dist/web/index.html" || {
    echo "no web client in dist/web; build it with scripts/ci/build-web.sh" >&2
    exit 1
}

export RUSTUP_HOME="${RUSTUP_HOME:-/opt/rust/rustup}" CARGO_HOME="${CARGO_HOME:-/opt/rust/cargo}"
export PATH="${CARGO_HOME}/bin:${PATH}"
libc=gnu
if ldd --version 2>&1 | grep -qi musl; then
    libc=musl
    # rustup's musl targets link the C runtime statically, which leaves a
    # binary unable to load the distribution's shared libraries
    export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-C target-feature=-crt-static"
fi
if ! command -v cargo > /dev/null; then
    url="https://static.rust-lang.org/rustup/dist/$(uname -m)-unknown-linux-${libc}/rustup-init"
    curl -fsSL --retry 5 -o /tmp/rustup-init "${url}"
    echo "$(curl -fsSL --retry 5 "${url}.sha256" | cut -d' ' -f1)  /tmp/rustup-init" | sha256sum -c -
    chmod +x /tmp/rustup-init
    /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "${RUST_TOOLCHAIN}"
    rm -f /tmp/rustup-init
fi

# shellcheck source=infra/packaging/version.sh
. "${REPO}/infra/packaging/version.sh"
CRATE_VERSION="$(crate_version "${SELKIES_VERSION:-0.0.0}")"
SRC="${BUILD}/server"
rm -rf "${SRC}"
mkdir -p "${SRC}"
cp -r "${REPO}/Cargo.toml" "${REPO}/Cargo.lock" "${REPO}/crates" "${SRC}/"
# The workspace version (the manifest's one top-level `version`) and the lock
# entries of its two members, which `--locked` holds to the manifest
sed -i "s|^version = .*|version = \"${CRATE_VERSION}\"|" "${SRC}/Cargo.toml"
for member in selkies selkies-rtc; do
    sed -i "/^name = \"${member}\"$/{n;s|^version = .*|version = \"${CRATE_VERSION}\"|}" "${SRC}/Cargo.lock"
done
cargo fetch --locked --manifest-path "${SRC}/Cargo.toml"

# The pixelflux checkout Cargo fetched at the commit the lock pins, whose wheel
# recipe and codec floors codecs.sh reads
PIXELFLUX="$(cargo metadata --locked --format-version 1 --manifest-path "${SRC}/Cargo.toml" \
    | grep -o '"manifest_path":"[^"]*/pixelflux/Cargo.toml"' | head -1 | cut -d'"' -f4)"
[ -n "${PIXELFLUX}" ] || { echo "cargo metadata names no pixelflux checkout" >&2; exit 1; }
"${REPO}/infra/packaging/codecs.sh" "${BUILD}/codecs" "$(dirname "${PIXELFLUX}")"
# shellcheck disable=SC1091  # written by codecs.sh just above
. "${BUILD}/codecs/env.sh"

if [ -n "${WHEEL_OUT}" ]; then
    cp -r "${REPO}/pyproject.toml" "${REPO}/README.md" "${REPO}/LICENSE" "${SRC}/"
    mkdir -p "${SRC}/infra" "${SRC}/build/wheel/data/data/share/selkies"
    cp -r "${REPO}/infra/wheel" "${SRC}/infra/wheel"
    cp -r "${REPO}/dist/web" "${SRC}/build/wheel/data/data/share/selkies/web"
    sed -i "s|^version =.*|version = \"${SELKIES_VERSION:-0.0.0.dev0}\"|" "${SRC}/pyproject.toml"
    # The tag the repair checks the wheel against, not a promise made here
    if [ "${libc}" = musl ]; then tag=musllinux_1_2; else tag=manylinux_2_28; fi
    (cd "${SRC}" && CARGO_TARGET_DIR="${BUILD}/target" maturin build --release --locked \
        --compatibility "${tag}" --auditwheel skip --out "${WHEEL_OUT}")
    if [ -n "${SELKIES_SDIST:-}" ]; then
        (cd "${SRC}" && maturin sdist --out "${WHEEL_OUT}")
    fi
    exit 0
fi

CARGO_TARGET_DIR="${BUILD}/target" cargo build --release --locked -p selkies \
    --manifest-path "${SRC}/Cargo.toml"

install -Dm755 "${BUILD}/target/release/selkies" "${PKG_ROOT}/usr/bin/selkies"
mkdir -p "${PKG_ROOT}/usr/share/selkies"
rm -rf "${PKG_ROOT}/usr/share/selkies/web"
cp -r "${REPO}/dist/web" "${PKG_ROOT}/usr/share/selkies/web"
# Fails the package build rather than shipping a server that cannot start, or
# one reporting a version other than the package's
"${PKG_ROOT}/usr/bin/selkies" --version | grep -F "selkies ${CRATE_VERSION} "
"${PKG_ROOT}/usr/bin/selkies" --help > /dev/null
