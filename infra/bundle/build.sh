#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the Selkies bundle (selkies-<version>-<arch>-linux-<gnu|musl>.tar.gz)
# for this runner's architecture and a C library into <out-dir>:
# infra/bundle/build-in-image.sh run inside the bundle's toolchain image for
# them (infra/bundle/Dockerfile), the manylinux_2_28 image for glibc, whose
# glibc, 2.28, is the glibc bundle's floor on both architectures, or the
# musllinux_1_2 one for musl, with everything the bundle carries that this tree
# does not compile already built into it. infra/bundle/toolchain.sh names the
# image for the files it is built from and takes it from this machine, else from
# the registry main publishes it to, else builds it, so a build compiles the
# server, the shell, and the interposers alone wherever the toolchain is
# unchanged. The web client the server embeds has to be in dist/web already
# (scripts/ci/build-web.sh). SELKIES_SOURCE_CACHE names a directory the sources
# fetched at build time are kept in between runs, and the build directory under
# build/bundle is kept between runs, so a rebuild compiles only what changed.
# SELKIES_CARGO_PROFILE names the Cargo profile the server and the shell are
# built with: release, the default and what a release ships, or iterate, which
# builds in a fraction of its time for trying a change.
#
# Usage: infra/bundle/build.sh <out-dir> [glibc|musl]
#   (SELKIES_VERSION stamps the version)
set -eux
OUT="$(mkdir -p "${1:?usage: build.sh <out-dir> [glibc|musl]}" && readlink -f "$1")"
LIBC="${2:-glibc}"
cd "$(readlink -f "$(dirname "$0")")/../.."
test -f dist/web/index.html
IMAGE="$(sh infra/bundle/toolchain.sh ensure "${LIBC}")"
WORK="${PWD}/build/bundle/${LIBC}"
mkdir -p "${WORK}/build" "${SELKIES_SOURCE_CACHE:=${WORK}/sources}"
docker run --rm -v "${PWD}:/repo:ro" -v "${WORK}/build:/build" -v "${OUT}:/out" \
    -v "$(readlink -f "${SELKIES_SOURCE_CACHE}"):/sources" -e SELKIES_SOURCE_CACHE=/sources \
    -e SELKIES_VERSION -e SELKIES_CARGO_PROFILE "${IMAGE}" sh -c \
    'sh /repo/infra/bundle/build-in-image.sh /out; status=$?; chown -R "$1" /out /build; exit "${status}"' \
    _ "$(id -u):$(id -g)"
