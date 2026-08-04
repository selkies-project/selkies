#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>_<arch>.deb (run inside the target apt-based distro container)
set -eux
apt-get clean && apt-get update
apt-get install --no-install-recommends -y python3 python3-venv python3-pip ruby ruby-dev build-essential ca-certificates
gem install --no-document fpm
rm -rf /pkg-root && mkdir -p /pkg-root/opt /pkg-root/usr/bin
/repo/infra/packaging/mkvenv.sh
DEB_ARCH="$(uname -m | sed -e 's/x86_64/amd64/' -e 's/aarch64/arm64/')"
fpm -s dir -t deb \
    --name selkies \
    --version "${SELKIES_VERSION:-0.0.0}" \
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
mkdir -p /out && mv selkies_*.deb /out/
