#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the Selkies bundle (selkies-<version>-<arch>-linux-<gnu|musl>.tar.gz)
# for this runner's architecture and a C library into <out-dir>:
# infra/bundle/build-in-image.sh run inside the manylinux_2_28 image, whose
# glibc, 2.28, is the glibc bundle's floor on both architectures (the X servers
# it carries need a newer toolchain than manylinux2014's), or the musllinux_1_2
# one for musl. The web client the server embeds has to be in dist/web
# already (scripts/ci/build-web.sh). The codecs are the glibc archives built
# once for the architecture (scripts/ci/build-codecs.sh) where SELKIES_CODECS_DIR
# names them, and built inside the image otherwise. SELKIES_SOURCE_CACHE names a
# directory the X servers' sources are kept in between runs. The image is
# infra/bundle/Dockerfile's, which Docker rebuilds only when its scripts change,
# and the build directory under build/bundle is kept between runs, so a rebuild
# compiles only what changed. SELKIES_CARGO_PROFILE names the Cargo profile the
# server and the shell are built with: release, the default and what a release
# ships, or iterate, which builds in a fraction of its time for trying a change.
#
# Usage: infra/bundle/build.sh <out-dir> [glibc|musl]
#   (SELKIES_VERSION stamps the version)
set -eux
OUT="$(mkdir -p "${1:?usage: build.sh <out-dir> [glibc|musl]}" && readlink -f "$1")"
LIBC="${2:-glibc}"
cd "$(readlink -f "$(dirname "$0")")/../.."
test -f dist/web/index.html
case "${LIBC}" in
    glibc) MANYLINUX="quay.io/pypa/manylinux_2_28_$(uname -m)" ;;
    musl) MANYLINUX="quay.io/pypa/musllinux_1_2_$(uname -m)" ;;
    *) echo "build.sh: no C library ${LIBC}" >&2; exit 2 ;;
esac
IMAGE="selkies-bundle-builder:${LIBC}-$(uname -m)"
WORK="${PWD}/build/bundle/${LIBC}"
mkdir -p "${WORK}/build" "${SELKIES_SOURCE_CACHE:=${WORK}/sources}"
set -- -v "${PWD}:/repo:ro" -v "${WORK}/build:/build" -v "${OUT}:/out" \
    -v "$(readlink -f "${SELKIES_SOURCE_CACHE}"):/sources" -e SELKIES_SOURCE_CACHE=/sources \
    -e SELKIES_VERSION -e SELKIES_CARGO_PROFILE
if [ -n "${SELKIES_CODECS_DIR:-}" ]; then
    set -- "$@" -v "$(readlink -f "${SELKIES_CODECS_DIR}"):/opt/selkies-codecs:ro" \
        -e SELKIES_CODECS=/opt/selkies-codecs
fi
scripts/ci/retry.sh 5 60 docker pull "${MANYLINUX}"
# The image's context is the scripts it copies, not the tree
tar -c infra/bundle/Dockerfile infra/bundle/deps.sh infra/packaging/portable-deps.sh \
    crates/pcmflux/scripts/build-libpulse.sh \
    | docker build -f infra/bundle/Dockerfile --build-arg MANYLINUX="${MANYLINUX}" -t "${IMAGE}" -
docker run --rm "$@" "${IMAGE}" sh -c \
    'sh /repo/infra/bundle/build-in-image.sh /out; status=$?; chown -R "$1" /out /build; exit "${status}"' \
    _ "$(id -u):$(id -g)"
