#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the codec libraries the bundle's server of one architecture and C
# library links, once, as static archives, for infra/bundle/build.sh to take
# from SELKIES_CODECS_DIR rather than build them itself.
#
# They are pixelflux's pinned releases with its patches, built in the image the
# bundle is built in, whose C library is the bundle's floor (glibc 2.28, musl
# 1.2). The prefix is /opt/selkies-codecs both here and where it is linked,
# because the pkg-config files it writes name it; the bundle's build mounts it
# there and names it in SELKIES_CODECS (infra/packaging/server.sh).
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
