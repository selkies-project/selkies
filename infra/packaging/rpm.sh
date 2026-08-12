#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>-1.<distro>.<arch>.rpm (run inside a Fedora/EL dnf-based
# container). DISTRO_TAG keeps the Fedora and EL flavors apart once the release
# job collects every package into a single directory.
set -eux
# python3-devel and the compilers: dependencies without a wheel for this
# distribution's Python are compiled from their sdist. libxkbcommon is loaded
# with ctypes at runtime and lets mkvenv.sh's smoke test exercise that path
dnf install -y \
    python3 python3-pip python3-devel \
    libxkbcommon pkgconf-pkg-config \
    ruby rubygems ruby-devel gcc gcc-c++ make rpm-build ca-certificates
# The i686 glibc builds the interposer's 32-bit variant, which the Wine and
# Steam catalog loads through `/usr/$LIB`
if [ "$(uname -m)" = "x86_64" ]; then
    dnf install -y glibc-devel.i686 libgcc.i686
fi
gem install --no-document fpm
. /repo/infra/packaging/version.sh
/repo/infra/packaging/mkvenv.sh
/repo/infra/packaging/interposer.sh /pkg-root
RPM_ARCH="$(uname -m)"
mkdir -p /out
cd /out
fpm -s dir -t rpm \
    --name selkies \
    --version "$(tilde_version "${SELKIES_VERSION:-0.0.0}")" \
    --iteration "1.${DISTRO_TAG:-linux}" \
    --architecture "${RPM_ARCH}" \
    --description "Low-latency HTML5 remote desktop streaming (WebSocket and WebRTC)" \
    --url "https://github.com/selkies-project/selkies" \
    --license "MPL-2.0" \
    --depends python3 \
    --depends pulseaudio-libs \
    --depends libxcb \
    --depends libxkbcommon \
    --depends libX11-xcb \
    --depends libva \
    --depends libdrm \
    --rpm-os linux \
    -C /pkg-root opt usr
ls -la /out
