#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install PipeWire's and SPA's headers from the release pinned here into
# <prefix>/include, for the webcam interposer's PipeWire frame source
# (addons/v4l2-interposer), which opens libpipewire at run time and needs
# nothing of it at build time but these. The build images' own are too old for
# it (AlmaLinux 8 ships PipeWire 0.3.6), or absent, and the headers are all the
# source tree supplies: the one generated header, the version, is written from
# its template here rather than by configuring the whole project.
#
# Runs in the bundle's build image, once into the toolchain image
# (infra/bundle/Dockerfile); a <prefix> holding the headers is left as it is.
#
# Usage: pipewire.sh <prefix>
#   writes <prefix>/include/pipewire-0.3 and <prefix>/include/spa-0.2, the
#   directories the interposer's PW_CFLAGS name
set -eux
PREFIX="$(mkdir -p "${1:?usage: pipewire.sh <prefix>}" && readlink -f "$1")"
[ ! -f "${PREFIX}/.complete" ] || exit 0
MAJOR=1 MINOR=4 MICRO=2
SHA256="4712aada64b9b49ad41fbb8b440914481432a560f2619ffbdd49461f8d22994f"
WORK="$(mktemp -d)"
curl -fsSL --retry 8 --retry-delay 30 --connect-timeout 30 -o "${WORK}/pipewire.tar.gz" \
    "https://gitlab.freedesktop.org/pipewire/pipewire/-/archive/${MAJOR}.${MINOR}.${MICRO}/pipewire-${MAJOR}.${MINOR}.${MICRO}.tar.gz"
echo "${SHA256}  ${WORK}/pipewire.tar.gz" | sha256sum -c -
mkdir -p "${WORK}/src" "${PREFIX}/include/pipewire-0.3" "${PREFIX}/include/spa-0.2"
tar -xzf "${WORK}/pipewire.tar.gz" -C "${WORK}/src" --strip-components=1
cp -r "${WORK}/src/src/pipewire" "${PREFIX}/include/pipewire-0.3/"
find "${PREFIX}/include/pipewire-0.3/pipewire" -type f ! -name '*.h' -delete
sed -e "s/@PIPEWIRE_VERSION_MAJOR@/${MAJOR}/g" -e "s/@PIPEWIRE_VERSION_MINOR@/${MINOR}/g" \
    -e "s/@PIPEWIRE_VERSION_MICRO@/${MICRO}/g" -e 's/@PIPEWIRE_API_VERSION@/"0.3"/g' \
    "${WORK}/src/src/pipewire/version.h.in" > "${PREFIX}/include/pipewire-0.3/pipewire/version.h"
if grep -q '@' "${PREFIX}/include/pipewire-0.3/pipewire/version.h"; then
    echo "pipewire: version.h names a variable this script does not fill" >&2
    exit 1
fi
cp -r "${WORK}/src/spa/include/spa" "${PREFIX}/include/spa-0.2/"
rm -rf "${WORK}"
touch "${PREFIX}/.complete"
