#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the codec libraries every server binary of one architecture and C
# library links, once, as the static archives the native packages and the
# AppImage all link in.
#
# They are pixelflux's pinned releases with its patches, so a build on each
# distribution would only compile the same archive again with another
# toolchain. This one builds them in the image the wheels are built in, whose
# C library is the oldest any package targets (glibc 2.17 on x86_64, 2.28 on
# aarch64, musl 1.2), so an archive from it links into a server built on any
# newer distribution. The prefix is /opt/selkies-codecs both here and where
# it is linked, because the pkg-config files it writes name it; a package
# build mounts it there and names it in SELKIES_CODECS (infra/packaging/server.sh).
# The container empties the prefix before it builds, since a build it was
# stopped in leaves files only its root user can remove.
#
# Usage: scripts/ci/build-codecs.sh <glibc|musl> [arch] [out-dir]
#   arch is a `uname -m` name and defaults to this host's; out-dir defaults to
#   build/codecs/<libc>-<arch> and receives the prefix.
set -eux
LIBC="${1:?usage: build-codecs.sh <glibc|musl> [arch] [out-dir]}"
ARCH="${2:-$(uname -m)}"
cd "$(readlink -f "$(dirname "$0")")/../.."
OUT="${3:-${PWD}/build/codecs/${LIBC}-${ARCH}}"
case "${LIBC}-${ARCH}" in
    glibc-x86_64) IMAGE="quay.io/pypa/manylinux2014_x86_64" ;;
    glibc-*) IMAGE="quay.io/pypa/manylinux_2_28_${ARCH}" ;;
    musl-*) IMAGE="quay.io/pypa/musllinux_1_2_${ARCH}" ;;
    *) echo "build-codecs: unknown C library ${LIBC}" >&2; exit 1 ;;
esac
mkdir -p "${OUT}"
OUT="$(readlink -f "${OUT}")"
scripts/ci/retry.sh 5 60 docker pull "${IMAGE}"
docker run --rm \
    -v "${PWD}:/repo:ro" \
    -v "${OUT}:/opt/selkies-codecs" \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    "${IMAGE}" \
    sh -euxc 'find /opt/selkies-codecs -mindepth 1 -delete
        /repo/infra/packaging/portable-deps.sh
        PATH="/opt/python/cp312-cp312/bin:${PATH}" \
            /repo/infra/packaging/codecs.sh /opt/selkies-codecs /repo/crates/pixelflux
        rm -rf /opt/selkies-codecs/src
        chown -R "${HOST_UID}:${HOST_GID}" /opt/selkies-codecs'
test -s "${OUT}/env.sh"
