#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies_<ver>-1~<distro>_<arch>.deb (run inside the target apt-based
# distro container). DISTRO_TAG keeps the Debian and Ubuntu flavors apart once
# the release job collects every package into a single directory.
set -eux
export DEBIAN_FRONTEND="noninteractive"
apt-get clean && apt-get update
# python3-dev and build-essential: dependencies without a wheel for this
# distribution's Python are compiled from their sdist. libxkbcommon0 is loaded
# with ctypes at runtime and lets mkvenv.sh's smoke test exercise that path
# libpipewire-0.3-dev gives the V4L2 interposer its PipeWire frame source
# (headers only; the library is loaded at runtime when an application uses it)
apt-get install --no-install-recommends -y \
    python3 python3-venv python3-pip python3-dev \
    libxkbcommon0 pkg-config libpipewire-0.3-dev \
    ruby ruby-dev build-essential ca-certificates
# gcc-multilib builds the interposer's 32-bit variant, which the Wine and Steam
# catalog loads through `/usr/$LIB`
if [ "$(dpkg --print-architecture)" = "amd64" ]; then
    apt-get install --no-install-recommends -y gcc-multilib
fi
gem install --no-document fpm
# shellcheck source=infra/packaging/version.sh
. /repo/infra/packaging/version.sh
/repo/infra/packaging/mkvenv.sh
/repo/infra/packaging/interposer.sh /pkg-root
/repo/infra/packaging/v4l2-interposer.sh /pkg-root
# dpkg knows the Debian name for whatever this is running on, so a new
# architecture needs no translation table here
DEB_ARCH="$(dpkg --print-architecture)"
mkdir -p /out
cd /out
fpm -s dir -t deb \
    --name selkies \
    --version "$(tilde_version "${SELKIES_VERSION:-0.0.0}")" \
    --iteration "1~${DISTRO_TAG:-linux}" \
    --architecture "${DEB_ARCH}" \
    --description "Low-latency HTML5 remote desktop streaming (WebSocket and WebRTC)" \
    --url "https://github.com/selkies-project/selkies" \
    --license "MPL-2.0" \
    --depends python3 \
    --depends libpulse0 \
    --depends libxcb1 \
    --depends libxkbcommon0 \
    --depends libx11-xcb1 \
    --depends libva2 \
    --depends libdrm2 \
    --depends libgbm1 \
    --depends libegl1 \
    --depends libglib2.0-0 \
    --depends libpixman-1-0 \
    --depends libxcb-render0 \
    --depends libxcb-shm0 \
    --depends libxcb-dri3-0 \
    --depends libxfixes3 \
    --depends libxext6 \
    -C /pkg-root opt usr
ls -la /out
