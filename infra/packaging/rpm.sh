#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>-1.<distro>.<arch>.rpm (run inside a Fedora/EL dnf-based
# container). DISTRO_TAG keeps the Fedora and EL flavors apart once the release
# job collects every package into a single directory.
set -eux

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
# EL keeps nasm, meson, and the input and Wayland headers in CRB, which only
# the distribution's own repositories reach; no third-party repository (RPM
# Fusion, EPEL) is added, so every codec library neither carries is built by
# codecs.sh and linked in. Fedora has no CRB and needs none.
if [ -e /etc/redhat-release ] && ! grep -q Fedora /etc/redhat-release; then
    retry dnf install -y dnf-plugins-core
    dnf config-manager --set-enabled crb
fi
# The server's build: the toolchain bindgen and the vendored C sources need,
# the headers of every library it links, and the codec headers the
# distribution carries, each tried on its own since a release may lack it
# (codecs.sh builds whatever is missing or older than pixelflux needs). pipewire-devel
# gives the V4L2 interposer its PipeWire frame source (headers only; the
# library is loaded at runtime when an application uses it)
retry dnf install -y \
    ca-certificates git gcc gcc-c++ make cmake nasm meson ninja-build clang clang-devel \
    pkgconf-pkg-config diffutils libva-devel libdrm-devel mesa-libgbm-devel mesa-libEGL-devel \
    wayland-devel libinput-devel systemd-devel libxkbcommon-devel pixman-devel libxcb-devel \
    pulseaudio-libs-devel pipewire-devel \
    ruby rubygems ruby-devel rpm-build
for package in libdav1d-devel; do
    dnf install -y "${package}" || echo "${package}: not in this distribution"
done
# The i686 glibc builds the interposer's 32-bit variant, which the Wine and
# Steam catalog loads through `/usr/$LIB`
if [ "$(uname -m)" = "x86_64" ]; then
    retry dnf install -y glibc-devel.i686 libgcc.i686
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
    "$@" \
    --rpm-os linux \
    -C /pkg-root usr
# Every package file is selkies-<version>-<distribution>-<architecture>.<format>, the
# version as the release tag spells it; rpm reads its own version and ordering
# (2.0.0~rc0-1.distro) from inside the archive.
mv /out/selkies-*.rpm "/out/selkies-${SELKIES_VERSION:-0.0.0}-${DISTRO_TAG:-linux}-${RPM_ARCH}.rpm"
ls -la /out
