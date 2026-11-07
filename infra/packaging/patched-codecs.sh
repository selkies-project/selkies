#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install the codec libraries pixelflux carries patches for, as the workspace
# pixelflux links them, under <prefix>: libx264 and libvpx, built by that
# checkout's scripts/build-x264.sh and scripts/build-libvpx.sh at the commits its
# wheel recipe pins with the patches applied, as static archives the server
# links in (SYSTEM_DEPS_X264_LINK=static, VPX_STATIC=1). For a build that does
# not go through codecs.sh, which builds them the same way. Cargo runs as the
# caller; SUDO, where set, runs the builds that install.
#
# Usage: [SUDO=sudo] patched-codecs.sh <prefix>
set -eu
PREFIX="${1:?usage: patched-codecs.sh <prefix>}"
cd "$(dirname "$(readlink -f "$0")")/../.."
cargo fetch --locked
PIXELFLUX="$(cargo metadata --locked --format-version 1 \
    | grep -o '"manifest_path":"[^"]*/pixelflux/Cargo.toml"' | head -1 | cut -d'"' -f4)"
[ -n "${PIXELFLUX}" ] || { echo "patched-codecs: cargo metadata names no pixelflux checkout" >&2; exit 1; }
for codec in x264 libvpx; do
    ${SUDO:-} sh "$(dirname "${PIXELFLUX}")/scripts/build-${codec}.sh" "${PREFIX}"
done
