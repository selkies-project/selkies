#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies_<ver>-1~<distro>_<arch>.deb (run inside the target apt-based
# distro container). DISTRO_TAG keeps the Debian and Ubuntu flavors apart once
# the release job collects every package into a single directory.
set -eux
export DEBIAN_FRONTEND="noninteractive"

# Package managers with no retry option of their own -- apk, dnf, pacman, and
# RubyGems all lack one -- are bounded-retried here. This composes with whatever
# internal retrying the tool already does rather than replacing it, so it cannot
# lower a default the way an explicit --setopt could.
retry() {
    i=1
    until "$@"; do
        [ "${i}" -ge 5 ] && return 1
        i=$((i + 1)); sleep 5
    done
}
# One transient index failure should not cost a package build that is already
# several minutes in
printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\nAcquire::Retries::Delay::Maximum "30";\n' > /etc/apt/apt.conf.d/99-selkies-retries
apt-get clean && apt-get update
# The server's build: the toolchain bindgen and the vendored C sources need,
# and the headers of every library it links. libpipewire-0.3-dev gives the V4L2
# interposer its PipeWire frame source (headers only; the library is loaded at
# runtime when an application uses it)
apt-get install --no-install-recommends -y \
    ca-certificates curl git build-essential pkg-config cmake nasm clang libclang-dev \
    libx264-dev libx265-dev libvpx-dev libsvtav1enc-dev libdav1d-dev libde265-dev \
    libva-dev libdrm-dev libgbm-dev libegl-dev libgles-dev libwayland-dev libinput-dev \
    libudev-dev libxkbcommon-dev libpixman-1-dev libxcb1-dev libxcb-shm0-dev \
    libxcb-xfixes0-dev libpulse-dev libpipewire-0.3-dev \
    ruby ruby-dev
# gcc-multilib builds the interposer's 32-bit variant, which the Wine and Steam
# catalog loads through `/usr/$LIB`
if [ "$(dpkg --print-architecture)" = "amd64" ]; then
    apt-get install --no-install-recommends -y gcc-multilib
fi
retry gem install --no-document fpm
# shellcheck source=infra/packaging/version.sh
. /repo/infra/packaging/version.sh
rm -rf /pkg-root
/repo/infra/packaging/server.sh /pkg-root
/repo/infra/packaging/interposer.sh /pkg-root
/repo/infra/packaging/v4l2-interposer.sh /pkg-root
# One --depends per package behind a library the server links or loads
DEPENDS="$(/repo/infra/packaging/runtime-deps.sh /pkg-root/usr/bin/selkies)"
set --
for package in ${DEPENDS}; do
    set -- "$@" --depends "${package}"
done
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
    "$@" \
    -C /pkg-root usr
# Every package file is selkies-<version>-<distribution>-<architecture>.<format>, the
# version as the release tag spells it; dpkg reads its own version and ordering
# (2.0.0~rc0-1~distro) from inside the archive.
mv /out/selkies_*.deb "/out/selkies-${SELKIES_VERSION:-0.0.0}-${DISTRO_TAG:-linux}-${DEB_ARCH}.deb"
ls -la /out
