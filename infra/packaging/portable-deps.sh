#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install what infra/packaging/server.sh needs to build the server inside a
# manylinux_2_28 or musllinux_1_2 image, the images pixelflux's and pcmflux's
# own wheels are built in: the selkies wheel is built there, and the AppImage's
# server on manylinux_2_28, whose glibc is its floor.
#
# The headers of every library the server links come from the image's own
# distribution, as pixelflux's wheels take them: AlmaLinux 8's BaseOS,
# AppStream, and PowerTools without the EPEL the image carries, or Alpine's
# main and community. No codec package is installed, so codecs.sh builds each
# from pixelflux's pins, and no libxkbcommon, which each caller links its own
# way. meson, for dav1d and libxkbcommon, comes from the interpreter the image
# ships rather than the distribution's, which is too old on AlmaLinux 8, with
# any other Python tool the caller names; the caller puts that interpreter's
# bin directory on PATH.
#
# Usage: portable-deps.sh [pip requirement...]
set -eux

# A mirror still syncing an update answers 404 for the repodata the index it
# was handed names, and dnf gives up once every mirror has, so the install is
# retried with fresh metadata. apk has no retry of its own.
retry_install() {
    for attempt in 1 2 3 4; do
        "$@" && return 0
        if command -v dnf > /dev/null; then dnf clean metadata; fi
        sleep $((attempt * 30))
    done
    return 1
}
if command -v dnf > /dev/null; then
    retry_install dnf install -y --disablerepo='epel*' git nasm bison clang clang-devel \
        pixman-devel libxcb-devel libdrm-devel libinput-devel mesa-libgbm-devel mesa-libEGL-devel \
        wayland-devel systemd-devel pulseaudio-libs-devel
else
    retry_install apk add --no-cache build-base pkgconf linux-headers git curl cmake nasm bison \
        clang pixman-dev libxcb-dev libdrm-dev libinput-dev mesa-dev wayland-dev eudev-dev \
        pulseaudio-dev
    # bindgen opens libclang at run time, and Alpine names that package after
    # the compiler's major version, which moves every release
    retry_install apk add --no-cache "clang$(clang -dumpversion | cut -d. -f1)-libclang"
fi
/opt/python/cp312-cp312/bin/pip install --retries 5 meson ninja "$@"
