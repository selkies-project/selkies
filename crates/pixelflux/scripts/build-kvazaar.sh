#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the release the wheel recipe pins. Kvazaar 2.3.2 moved rate control
# into each encoder, the ABI codec-sys requires, and Ubuntu 24.04 has no package.
#
# Usage: build-kvazaar.sh <prefix>
set -eu
PREFIX="${1:?usage: build-kvazaar.sh <prefix>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$(mktemp -d)"
trap 'rm -rf "${SRC}"' EXIT

# The commit, release tag, and source the recipe's clone_source names.
# shellcheck disable=SC2046  # awk emits the three values as separate arguments
set -- $(awk '
    $1 == "clone_source" && $4 == "kvazaar" { commit = $2; tag = $3; next }
    commit != "" { sub(/ *&&.*/, ""); sub(/^ +/, ""); print commit " " tag " " $0; exit }
' "${ROOT}/pyproject.toml")
COMMIT="$1"
TAG="$2"
URL="$3"

git -C "${SRC}" init -q
git -C "${SRC}" fetch -q --depth 1 "${URL}" "refs/tags/${TAG}:refs/tags/${TAG}"
GOT="$(git -C "${SRC}" rev-parse 'FETCH_HEAD^{commit}')"
[ "${GOT}" = "${COMMIT}" ] || { echo "build-kvazaar: ${TAG} is ${GOT}, not ${COMMIT}" >&2; exit 1; }
git -C "${SRC}" checkout -q "${COMMIT}"
cmake -S "${SRC}" -B "${SRC}/build" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="${PREFIX}" -DCMAKE_INSTALL_LIBDIR=lib -DBUILD_SHARED_LIBS=ON
cmake --build "${SRC}/build" -j"$(nproc 2>/dev/null || echo 4)"
cmake --install "${SRC}/build"
