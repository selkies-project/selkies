#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build libx264 as pixelflux links it: the commit the wheel recipe pins
# (pyproject.toml), with the patches under patches/x264 applied, installed
# under <prefix>. pixelflux reads the API those patches add, so a distribution's
# libx264 does not build it; every build that is not the wheel's takes this one.
#
# Usage: build-x264.sh <prefix> [static|shared]
#   static (the default) installs an archive with position-independent code,
#   which pixelflux links into itself when SYSTEM_DEPS_X264_LINK=static.
set -eu
PREFIX="${1:?usage: build-x264.sh <prefix> [static|shared]}"
KIND="${2:-static}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$(mktemp -d)"
trap 'rm -rf "${SRC}"' EXIT

# The commit and mirrors the recipe's clone_source names for x264.
# shellcheck disable=SC2046  # awk emits the commit and URLs as separate arguments
set -- $(awk '
    $1 == "clone_source" && $4 == "x264" { commit = $2; next }
    commit != "" { sub(/ *&&.*/, ""); sub(/^ +/, ""); print commit " " $0; exit }
' "${ROOT}/pyproject.toml")
COMMIT="$1"
shift
fetched=0
for url in "$@"; do
    if git -C "${SRC}" init -q && git -C "${SRC}" fetch -q --depth 1 "${url}" "${COMMIT}"; then
        fetched=1
        break
    fi
done
[ "${fetched}" = 1 ] || { echo "build-x264: no mirror served ${COMMIT}" >&2; exit 1; }
git -C "${SRC}" checkout -q "${COMMIT}"
git -C "${SRC}" apply "${ROOT}"/patches/x264/*.patch

case "${KIND}" in
    static) flags="--enable-static --enable-pic" ;;
    shared) flags="--enable-shared" ;;
    *) echo "build-x264: unknown kind ${KIND}" >&2; exit 1 ;;
esac
# shellcheck disable=SC2086  # flags is a list of options
(cd "${SRC}" && ./configure --prefix="${PREFIX}" ${flags} --disable-cli --disable-opencl \
    && make -j"$(nproc 2>/dev/null || echo 4)" && make install)
