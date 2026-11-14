#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Stamp a release's version into every public crate of a copy of the tree.
#
# usage: stamp-version.sh <tree> <version>
#
# The tree versions its crates 0.0.0 (every workspace member that takes the
# workspace's version, and pixelflux and pcmflux), and a
# build of a release writes its PEP 440 version (2.3.0, 2.3.0rc0) into them in
# Cargo's semver spelling: the workspace manifest and its lock, and the manifest
# and lock each native crate keeps for its standalone wheel build, so the root
# lock covers both that build and the workspace build under `--locked`. Fails
# when any of them does not read the version afterwards.
set -eu

TREE="$1"
# shellcheck source=infra/packaging/version.sh
. "$(dirname "$0")/version.sh"
CRATE_VERSION="$(crate_version "$2")"

# Rewrite $1 through sed expression $2 only where that changes it, so a file that
# already reads the version keeps its time and Cargo the build it made from it
rewrite() {
    sed "$2" "$1" > "$1.stamp"
    if cmp -s "$1" "$1.stamp"; then rm -f "$1.stamp"; else mv "$1.stamp" "$1"; fi
}
stamp_manifest() {
    rewrite "$1" "1,/^version = /s|^version = .*|version = \"${CRATE_VERSION}\"|"
    grep -q "^version = \"${CRATE_VERSION}\"$" "$1" || {
        echo "stamp-version.sh: $1 does not read ${CRATE_VERSION}" >&2
        exit 1
    }
}

stamp_lock() {
    rewrite "$1" "/^name = \"$2\"$/{n;s|^version = .*|version = \"${CRATE_VERSION}\"|}"
    grep -A1 "^name = \"$2\"$" "$1" | grep -q "^version = \"${CRATE_VERSION}\"$" || {
        echo "stamp-version.sh: $2 in $1 does not read ${CRATE_VERSION}" >&2
        exit 1
    }
}

stamp_manifest "${TREE}/Cargo.toml"
# Every member that takes the workspace's version, then the two components that
# carry their own
for manifest in "${TREE}"/crates/*/Cargo.toml; do
    if grep -q '^version.workspace = true$' "${manifest}"; then
        stamp_lock "${TREE}/Cargo.lock" "$(sed -n 's/^name = "\(.*\)"$/\1/p' "${manifest}" | head -n 1)"
    fi
done
for member in pixelflux pcmflux; do
    stamp_lock "${TREE}/Cargo.lock" "${member}"
done
for component in pixelflux pcmflux; do
    stamp_manifest "${TREE}/crates/${component}/Cargo.toml"
    stamp_lock "${TREE}/crates/${component}/Cargo.lock" "${component}"
done
