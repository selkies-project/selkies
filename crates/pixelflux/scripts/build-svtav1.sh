#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build SVT-AV1 as pixelflux links it: the release the wheel recipe pins
# (pyproject.toml), with the patches under patches/svtav1 applied, installed
# under <prefix>. pixelflux reads the API those patches add (the constant-block
# map a picture carries), so a distribution's SVT-AV1 does not build it; every
# build that is not the wheel's takes this one.
#
# Usage: build-svtav1.sh <prefix> [static|shared]
#   static (the default) installs an archive with position-independent code,
#   which pixelflux links into itself when SVTAV1ENC_STATIC=1.
set -eu
PREFIX="${1:?usage: build-svtav1.sh <prefix> [static|shared]}"
KIND="${2:-static}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$(mktemp -d)"
trap 'rm -rf "${SRC}"' EXIT

# The commit, tag, and mirrors the recipe's clone_source names for svtav1.
# shellcheck disable=SC2046  # awk emits the commit, tag, and URLs as separate arguments
set -- $(awk '
    $1 == "clone_source" && $4 == "svtav1" { commit = $2; tag = $3; next }
    commit != "" { sub(/ *&&.*/, ""); sub(/^ +/, ""); print commit " " tag " " $0; exit }
' "${ROOT}/pyproject.toml")
COMMIT="$1"
TAG="$2"
shift 2
fetched=0
for url in "$@"; do
    if git -C "${SRC}" init -q && git -C "${SRC}" fetch -q --depth 1 "${url}" "refs/tags/${TAG}:refs/tags/${TAG}"; then
        got="$(git -C "${SRC}" rev-parse "FETCH_HEAD^{commit}")"
        [ "${got}" = "${COMMIT}" ] || { echo "build-svtav1: ${url} has ${TAG} at ${got}, not ${COMMIT}" >&2; exit 1; }
        fetched=1
        break
    fi
done
[ "${fetched}" = 1 ] || { echo "build-svtav1: no mirror served ${TAG}" >&2; exit 1; }
git -C "${SRC}" checkout -q "${COMMIT}"
git -C "${SRC}" apply "${ROOT}"/patches/svtav1/*.patch

case "${KIND}" in
    static) shared=OFF ;;
    shared) shared=ON ;;
    *) echo "build-svtav1: unknown kind ${KIND}" >&2; exit 1 ;;
esac
# Interprocedural optimization off: the archive would otherwise hold the
# compiler's intermediate code, which the linker Rust drives cannot read.
cmake -S "${SRC}" -B "${SRC}/build" -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="${PREFIX}" \
    -DCMAKE_INSTALL_LIBDIR=lib -DBUILD_SHARED_LIBS="${shared}" -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DCMAKE_INTERPROCEDURAL_OPTIMIZATION=OFF -DBUILD_APPS=OFF -DBUILD_DEC=OFF
cmake --build "${SRC}/build" -j"$(nproc 2>/dev/null || echo 4)"
cmake --install "${SRC}/build"
