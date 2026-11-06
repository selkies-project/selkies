#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install libx264 as the workspace pixelflux links it, under <prefix>: built by
# that checkout's scripts/build-x264.sh, with the patches whose API pixelflux
# reads, as a static archive the server links in (SYSTEM_DEPS_X264_LINK=static).
# For a build that does not go through codecs.sh, which builds it the same way.
# Cargo runs as the caller; SUDO, where set, runs the build that installs.
#
# Usage: [SUDO=sudo] x264.sh <prefix>
set -eu
PREFIX="${1:?usage: x264.sh <prefix>}"
cd "$(dirname "$(readlink -f "$0")")/../.."
cargo fetch --locked
PIXELFLUX="$(cargo metadata --locked --format-version 1 \
    | grep -o '"manifest_path":"[^"]*/pixelflux/Cargo.toml"' | head -1 | cut -d'"' -f4)"
[ -n "${PIXELFLUX}" ] || { echo "x264: cargo metadata names no pixelflux checkout" >&2; exit 1; }
${SUDO:-} sh "$(dirname "${PIXELFLUX}")/scripts/build-x264.sh" "${PREFIX}"
