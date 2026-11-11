#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build x265 as pixelflux links it: the release the wheel recipe pins
# (pyproject.toml), with the patches under patches/x265 applied, its 10-bit
# encoder linked into the 8-bit library so x265_api_get(10) answers, installed
# under <prefix>. pixelflux reads the API those patches add (the input layout it
# encodes in place, the constant-block map, qp-offsets), so a distribution's
# x265 does not build it; every build that is not the wheel's takes this one.
#
# Usage: build-x265.sh <prefix> [static|shared]
#   static (the default) installs an archive with position-independent code,
#   which pixelflux links into itself when X265_STATIC=1.
set -eu
PREFIX="${1:?usage: build-x265.sh <prefix> [static|shared]}"
KIND="${2:-static}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$(mktemp -d)"
trap 'rm -rf "${SRC}"' EXIT
JOBS="$(nproc 2>/dev/null || echo 4)"

# The commit, tag, and mirrors the recipe's clone_source names for x265.
# shellcheck disable=SC2046  # awk emits the commit, tag, and URLs as separate arguments
set -- $(awk '
    $1 == "clone_source" && $4 == "x265" { commit = $2; tag = $3; next }
    commit != "" { sub(/ *&&.*/, ""); sub(/^ +/, ""); print commit " " tag " " $0; exit }
' "${ROOT}/pyproject.toml")
COMMIT="$1"
TAG="$2"
shift 2
fetched=0
for url in "$@"; do
    if git -C "${SRC}" init -q && git -C "${SRC}" fetch -q --depth 1 "${url}" "refs/tags/${TAG}:refs/tags/${TAG}"; then
        got="$(git -C "${SRC}" rev-parse "FETCH_HEAD^{commit}")"
        [ "${got}" = "${COMMIT}" ] || { echo "build-x265: ${url} has ${TAG} at ${got}, not ${COMMIT}" >&2; exit 1; }
        fetched=1
        break
    fi
done
[ "${fetched}" = 1 ] || { echo "build-x265: no mirror served ${TAG}" >&2; exit 1; }
git -C "${SRC}" checkout -q "${COMMIT}"
git -C "${SRC}" apply "${ROOT}"/patches/x265/*.patch

case "${KIND}" in
    static) shared=OFF ;;
    shared) shared=ON ;;
    *) echo "build-x265: unknown kind ${KIND}" >&2; exit 1 ;;
esac
# The 10-bit encoder first, as a static archive the 8-bit build links in.
cmake -S "${SRC}/source" -B "${SRC}/build10" -DCMAKE_BUILD_TYPE=Release -DHIGH_BIT_DEPTH=ON \
    -DEXPORT_C_API=OFF -DENABLE_SHARED=OFF -DENABLE_CLI=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON
cmake --build "${SRC}/build10" -j"${JOBS}"
mkdir -p "${SRC}/build"
cp "${SRC}/build10/libx265.a" "${SRC}/build/libx265_main10.a"
cmake -S "${SRC}/source" -B "${SRC}/build" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="${PREFIX}" -DCMAKE_INSTALL_LIBDIR=lib -DENABLE_SHARED="${shared}" \
    -DENABLE_CLI=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON -DEXTRA_LIB=x265_main10.a \
    -DEXTRA_LINK_FLAGS="-L${SRC}/build" -DLINKED_10BIT=ON
# With the shared library as well, the static one first: on aarch64 both list the same
# assembly objects, whose rules two parallel targets would otherwise run at once.
[ "${shared}" = OFF ] || cmake --build "${SRC}/build" --target x265-static -j"${JOBS}"
cmake --build "${SRC}/build" -j"${JOBS}"
cmake --install "${SRC}/build"
if [ "${KIND}" = static ]; then
    # One archive carrying both depths, as the wheels link it.
    (cd "${SRC}/build" && printf 'CREATE libx265_all.a\nADDLIB %s/lib/libx265.a\nADDLIB libx265_main10.a\nSAVE\nEND\n' \
        "${PREFIX}" | ar -M && mv libx265_all.a "${PREFIX}/lib/libx265.a")
fi
