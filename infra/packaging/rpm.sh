#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>.<arch>.rpm (run inside a Fedora/EL dnf-based container)
set -eux
dnf install -y python3 python3-pip ruby rubygems ruby-devel gcc gcc-c++ make rpm-build ca-certificates
gem install --no-document fpm
rm -rf /pkg-root && mkdir -p /pkg-root/opt /pkg-root/usr/bin
/repo/infra/packaging/mkvenv.sh
RPM_ARCH="$(uname -m)"
fpm -s dir -t rpm \
    --name selkies \
    --version "${SELKIES_VERSION:-0.0.0}" \
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
mkdir -p /out && mv selkies-*.rpm /out/
