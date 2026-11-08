#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# pixelflux's and pcmflux's own license gates, each on a copy of its crate as
# its wheels build it: outside the workspace, its features are its own rather
# than unified with the server's, and its own Cargo.lock resolves the graph.
# pixelflux's scripts/check-licenses.py audits both of its builds, and each
# crate's deny.toml is checked with cargo-deny: pixelflux's non-GPL build to
# permissive licenses, its GPL build's licenses and sources, pcmflux to
# permissive licenses, and the advisories of both.
#
# Usage: scripts/ci/crate-licenses.sh   (from the repository root)

set -eu

HERE="$(dirname "$(readlink -f "$0")")"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
git archive HEAD crates/pixelflux crates/pcmflux | tar -x -C "${work}"

cd "${work}/crates/pixelflux"
python3 scripts/check-licenses.py
CARGO_DENY_ARGS=--exclude-dev "${HERE}/cargo-deny.sh" licenses bans sources
CARGO_DENY_ARGS="--exclude-dev --features gpl" "${HERE}/cargo-deny.sh" licenses sources advisories

cd "${work}/crates/pcmflux"
CARGO_DENY_ARGS=--exclude-dev "${HERE}/cargo-deny.sh" licenses bans sources advisories
