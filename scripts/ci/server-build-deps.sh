#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install what building the server from this checkout takes on an apt-based
# host, for clippy and `cargo test --workspace` outside a package build: the
# toolchain the images build it with, with rustfmt and clippy, and the headers
# of every library it links, the list the base image's `server` stage installs.
#
# The toolchain is read from addons/base/Dockerfile's RUST_TOOLCHAIN, so this
# cannot drift from the images and packages (tests/unit/test_package_deps.py
# holds those two together). A host with rustup (a GitHub runner) installs it
# there; any other gets rustup-init, checked against the hash rustup publishes
# beside it, which puts cargo in ${CARGO_HOME:-~/.cargo}/bin. The codec
# libraries are the distribution's: pixelflux's codec-sys names their floors,
# and a release that falls short fails the build at pkg-config, naming the
# library. libx264 is the exception, built under /usr/local with the patches
# whose API the pinned pixelflux reads (infra/packaging/x264.sh). Root runs
# apt-get and that install itself, anyone else through sudo.
#
# Usage: server-build-deps.sh
set -eu
REPO="$(dirname "$(readlink -f "$0")")/../.."
RUST_TOOLCHAIN="${RUST_TOOLCHAIN:-$(sed -n 's/^ARG RUST_TOOLCHAIN="\(.*\)"$/\1/p' "${REPO}/addons/base/Dockerfile")}"
[ -n "${RUST_TOOLCHAIN}" ] || { echo "addons/base/Dockerfile names no RUST_TOOLCHAIN" >&2; exit 1; }

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO=sudo
export DEBIAN_FRONTEND=noninteractive
${SUDO} apt-get update
${SUDO} apt-get install -y --no-install-recommends \
    ca-certificates curl git build-essential pkg-config cmake nasm clang libclang-dev \
    libx265-dev libvpx-dev libsvtav1enc-dev libdav1d-dev libde265-dev libopus-dev \
    libva-dev libdrm-dev libgbm-dev libegl-dev libgles-dev libwayland-dev libinput-dev \
    libudev-dev libxkbcommon-dev libpixman-1-dev libxcb1-dev libxcb-shm0-dev \
    libxcb-xfixes0-dev libpulse-dev

PATH="${CARGO_HOME:-${HOME}/.cargo}/bin:${PATH}"
if command -v rustup > /dev/null; then
    rustup toolchain install "${RUST_TOOLCHAIN}" --profile minimal --component rustfmt,clippy
    rustup default "${RUST_TOOLCHAIN}"
else
    url="https://static.rust-lang.org/rustup/dist/$(uname -m)-unknown-linux-gnu/rustup-init"
    work="$(mktemp -d)"
    trap 'rm -rf "${work}"' EXIT
    curl -fsSL --retry 5 -o "${work}/rustup-init" "${url}"
    echo "$(curl -fsSL --retry 5 "${url}.sha256" | cut -d' ' -f1)  ${work}/rustup-init" | sha256sum -c -
    chmod +x "${work}/rustup-init"
    "${work}/rustup-init" -y --profile minimal --default-toolchain "${RUST_TOOLCHAIN}" \
        --component rustfmt,clippy
fi
cargo --version
SUDO="${SUDO}" "${REPO}/infra/packaging/x264.sh" /usr/local
