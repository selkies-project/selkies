#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install everything the bundle's build takes from its manylinux_2_28 or
# musllinux_1_2 image rather than from this tree: what the server's build needs
# (infra/packaging/portable-deps.sh, PulseAudio's client libraries among it),
# the PulseAudio server of that same release, the tools and headers the X
# servers build against, the distribution's dbus-daemon, and, on glibc, a
# 32-bit toolchain for the interposers. infra/bundle/Dockerfile runs it once into an
# image the build then runs in, so the tree's sources are all a rebuild
# compiles; build-in-image.sh runs it itself in a bare manylinux image.
#
# Usage: deps.sh
set -eux
REPO="$(dirname "$(readlink -f "$0")")/../.."

"${REPO}/infra/packaging/portable-deps.sh"
# Where the image's distribution puts libraries: AlmaLinux's lib64, Alpine's lib
libdir=lib64
[ -d /usr/lib64 ] && [ ! -L /usr/lib64 ] || libdir=lib
PATH="/opt/python/cp312-cp312/bin:${PATH}" sh "${REPO}/crates/pcmflux/scripts/build-libpulse.sh" \
    /usr "${libdir}" daemon
if command -v dnf > /dev/null; then
    dnf install -y --disablerepo='epel*' bison flex expat-devel mesa-libgbm-devel \
        mesa-libEGL-devel mesa-libGL-devel libxslt gettext perl-XML-Parser xz dbus-daemon
    dnf install -y --disablerepo='epel*' glibc-devel.i686 libgcc.i686 || true
    dnf clean all
    ldconfig
else
    apk add --no-cache bison flex expat-dev mesa-dev libxslt gettext perl-xml-parser xz dbus \
        linux-headers patch
fi
