#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build out/selkies-<ver>-<arch>.AppImage on the runner for this architecture:
# linuxdeploy is fetched here, checked against the digest GitHub records for
# it, and infra/appimage/build.sh assembles the AppImage inside a
# manylinux_2_28 container, whose glibc is the AppImage's floor, with this
# checkout mounted read-only and the network reachable, for Cargo to fetch the
# pixelflux and pcmflux commits the lock pins. The web client has to be in
# dist/web already (scripts/ci/build-web.sh). The result is then checked from
# an extracted copy by scripts/ci/verify-appimage.sh.
#
# Usage: scripts/ci/appimage.sh [arch]   (any `uname -m` name; defaults to this host)

set -eux

ARCH="${1:-$(uname -m)}"

cd "$(readlink -f "$(dirname "$0")")/../.."
WORK="${PWD}/build/appimage"
rm -rf "${WORK}"
mkdir -p "${WORK}" out
test -f dist/web/index.html

# The latest published build: upstream publishes no release artifacts
addons/base/fetch-verified release linuxdeploy/linuxdeploy continuous \
    "linuxdeploy-${ARCH}.AppImage" "${WORK}/linuxdeploy.AppImage"
chmod +x "${WORK}/linuxdeploy.AppImage"

docker run --rm \
    -v "${PWD}:/repo:ro" \
    -v "${WORK}:/work" \
    -v "${PWD}/out:/out" \
    -e SELKIES_VERSION -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    "quay.io/pypa/manylinux_2_28_${ARCH}" \
    sh /repo/infra/appimage/build.sh

OUTPUT="out/selkies-${SELKIES_VERSION:-0.0.0}-${ARCH}.AppImage"
scripts/ci/verify-appimage.sh "${OUTPUT}"
ls -la out/
