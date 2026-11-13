#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the Selkies server and stage it under a root at usr/bin/selkies, with
# the web client compiled into it: the bundle's server (infra/bundle), built in
# the manylinux_2_28 or musllinux_1_2 image whose C library is the bundle's
# floor. The codec libraries are the static archives built for the
# architecture and C library (codecs.sh, scripts/ci/build-codecs.sh), whose
# prefix SELKIES_CODECS names, and a build refuses to start without them. The
# toolchain is the pinned release the images
# build with, installed by rustup-init checked against the hash rustup
# publishes beside it. pixelflux and pcmflux are copied with the workspace and
# built through local path dependencies. The checkout stays read-only:
# the manifests are copied out so the release version can be stamped into
# them, and everything Cargo writes goes under /build.
#
# The web client is built once per run by scripts/ci/build-web.sh into
# dist/web and copied beside the crates, where the server's build script
# embeds it (crates/selkies/build.rs), so every channel carries it as it is.
# SELKIES_WEB_DIST names the copy, which makes a build that finds no client
# fail, and a package's server that does not report it embedded fails too.
#
# Usage: SELKIES_CODECS=<prefix> server.sh <pkg-root>
#                                      (SELKIES_VERSION stamps the version)
set -eux
PKG_ROOT="${1:?usage: server.sh <pkg-root>}"
[ -f "${SELKIES_CODECS:-}/env.sh" ] || {
    echo "no codec prefix in SELKIES_CODECS; build one with scripts/ci/build-codecs.sh" >&2
    exit 1
}
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
cp -a "${REPO}/Cargo.toml" "${REPO}/Cargo.lock" "${REPO}/crates" "${SRC}/"
mkdir -p "${SRC}/dist"
cp -a "${REPO}/dist/web" "${SRC}/dist/web"
chmod -R u+w "${SRC}"
export SELKIES_WEB_DIST="${SRC}/dist/web"
# Every public crate carries the release version
sh "${REPO}/infra/packaging/stamp-version.sh" "${SRC}" "${SELKIES_VERSION:-0.0.0}"
cargo fetch --locked --manifest-path "${SRC}/Cargo.toml"

# shellcheck disable=SC1091  # written by codecs.sh
. "${SELKIES_CODECS}/env.sh"

PROFILE="${SELKIES_CARGO_PROFILE:-release}"
CARGO_TARGET_DIR="${BUILD}/target" cargo build --profile "${PROFILE}" --locked -p selkies \
    --manifest-path "${SRC}/Cargo.toml"

install -Dm755 "${BUILD}/target/${PROFILE}/selkies" "${PKG_ROOT}/usr/bin/selkies"
# Fails the package build rather than shipping a server that cannot start, one
# reporting a version other than the package's, or one without its client
"${PKG_ROOT}/usr/bin/selkies" --version | grep -F "selkies ${CRATE_VERSION} (pixelflux ${CRATE_VERSION} ("
"${PKG_ROOT}/usr/bin/selkies" --version | grep -F ", pcmflux ${CRATE_VERSION})"
"${PKG_ROOT}/usr/bin/selkies" --version | grep -F "web client: embedded ("
"${PKG_ROOT}/usr/bin/selkies" --help > /dev/null
