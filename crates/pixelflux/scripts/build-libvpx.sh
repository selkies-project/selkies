#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build libvpx as pixelflux links it: the release the wheel recipe pins
# (pyproject.toml), with the patches under patches/libvpx applied, installed
# under <prefix>. The patches copy only the blocks a frame's active map leaves
# active into the encoder's own buffer, so a distribution's libvpx would code the
# same stream a copy of the whole picture slower; every build that is not the
# wheel's takes this one.
#
# Usage: build-libvpx.sh <prefix> [static|shared]
#   static (the default) installs an archive with position-independent code,
#   which pixelflux links into itself when VPX_STATIC=1.
set -eu
PREFIX="${1:?usage: build-libvpx.sh <prefix> [static|shared]}"
KIND="${2:-static}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$(mktemp -d)"
trap 'rm -rf "${SRC}"' EXIT

# The commit, tag, and mirrors the recipe's clone_source names for libvpx.
# shellcheck disable=SC2046  # awk emits the commit, tag, and URLs as separate arguments
set -- $(awk '
    $1 == "clone_source" && $4 == "libvpx" { commit = $2; tag = $3; next }
    commit != "" { sub(/ *&&.*/, ""); sub(/^ +/, ""); print commit " " tag " " $0; exit }
' "${ROOT}/pyproject.toml")
COMMIT="$1"
TAG="$2"
shift 2
fetched=0
for url in "$@"; do
    if git -C "${SRC}" init -q && git -C "${SRC}" fetch -q --depth 1 "${url}" "refs/tags/${TAG}:refs/tags/${TAG}"; then
        got="$(git -C "${SRC}" rev-parse "FETCH_HEAD^{commit}")"
        [ "${got}" = "${COMMIT}" ] || { echo "build-libvpx: ${url} has ${TAG} at ${got}, not ${COMMIT}" >&2; exit 1; }
        fetched=1
        break
    fi
done
[ "${fetched}" = 1 ] || { echo "build-libvpx: no mirror served ${TAG}" >&2; exit 1; }
git -C "${SRC}" checkout -q "${COMMIT}"
git -C "${SRC}" apply "${ROOT}"/patches/libvpx/*.patch

case "${KIND}" in
    static) flags="--enable-static --disable-shared --enable-pic" ;;
    shared) flags="--enable-shared --disable-static" ;;
    *) echo "build-libvpx: unknown kind ${KIND}" >&2; exit 1 ;;
esac
as=""
[ "$(uname -m)" != x86_64 ] || as="--as=nasm"
# shellcheck disable=SC2086  # flags and as are lists of options
(cd "${SRC}" && ./configure --prefix="${PREFIX}" ${flags} --disable-examples --disable-tools \
    --disable-docs --disable-unit-tests --enable-vp9-highbitdepth ${as} \
    && make -j"$(nproc 2>/dev/null || echo 4)" && make install)
