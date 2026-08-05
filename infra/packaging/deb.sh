#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies_<ver>-1~<distro>_<arch>.deb (run inside the target apt-based
# distro container). DISTRO_TAG keeps the Debian and Ubuntu flavors apart once
# the release job collects every package into a single directory.
set -eux
export DEBIAN_FRONTEND=noninteractive
apt-get clean && apt-get update
# python3-dev and build-essential: dependencies without a wheel for this
# distribution's Python are compiled from their sdist. libxkbcommon0 is loaded
# with ctypes at runtime and lets mkvenv.sh's smoke test exercise that path
apt-get install --no-install-recommends -y \
    python3 python3-venv python3-pip python3-dev \
    libxkbcommon0 pkg-config \
    ruby ruby-dev build-essential ca-certificates
gem install --no-document fpm
/repo/infra/packaging/mkvenv.sh
DEB_ARCH="$(uname -m | sed -e 's/x86_64/amd64/' -e 's/aarch64/arm64/')"
mkdir -p /out
cd /out
fpm -s dir -t deb \
    --name selkies \
    --version "${SELKIES_VERSION:-0.0.0}" \
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
    -C /pkg-root opt usr
ls -la /out
