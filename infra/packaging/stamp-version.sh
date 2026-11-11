#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Stamp a release's version into every public crate of a copy of the tree.
#
# usage: stamp-version.sh <tree> <version>
#
# The tree versions its crates 0.0.0 (selkies, selkies-rtc, selkies-wm,
# selkies-draw, selkies-desktop, pixelflux, and pcmflux), and a
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

stamp_manifest() {
    sed -i "1,/^version = /s|^version = .*|version = \"${CRATE_VERSION}\"|" "$1"
    grep -q "^version = \"${CRATE_VERSION}\"$" "$1" || {
        echo "stamp-version.sh: $1 does not read ${CRATE_VERSION}" >&2
        exit 1
    }
}

stamp_lock() {
    sed -i "/^name = \"$2\"$/{n;s|^version = .*|version = \"${CRATE_VERSION}\"|}" "$1"
    grep -A1 "^name = \"$2\"$" "$1" | grep -q "^version = \"${CRATE_VERSION}\"$" || {
        echo "stamp-version.sh: $2 in $1 does not read ${CRATE_VERSION}" >&2
        exit 1
    }
}

stamp_manifest "${TREE}/Cargo.toml"
for member in selkies selkies-rtc selkies-wm selkies-draw selkies-desktop pixelflux pcmflux; do
    stamp_lock "${TREE}/Cargo.lock" "${member}"
done
for component in pixelflux pcmflux; do
    stamp_manifest "${TREE}/crates/${component}/Cargo.toml"
    stamp_lock "${TREE}/crates/${component}/Cargo.lock" "${component}"
done
