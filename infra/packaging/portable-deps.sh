#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install what infra/packaging/server.sh needs to build the server inside a
# manylinux2014 (x86_64), manylinux_2_28 (aarch64), or musllinux_1_2 image,
# the images pixelflux's and pcmflux's own wheels are built in: the selkies
# wheel and the AppImage's server are built there, the image's glibc being
# their floor.
#
# The headers of every library the server links come from the image's own
# distribution, as pixelflux's wheels take them: CentOS 7's base and updates,
# or AlmaLinux 8's BaseOS, AppStream, and PowerTools, without the EPEL either
# image carries, or Alpine's main and community. No codec package is installed, so codecs.sh builds each
# from pixelflux's pins, and no libxkbcommon, which each caller links its own
# way. meson, for dav1d and libxkbcommon, comes from the interpreter the image
# ships rather than the distribution's, which is too old on AlmaLinux 8, with
# any other Python tool the caller names; the caller puts that interpreter's
# bin directory on PATH.
#
# Usage: portable-deps.sh [pip requirement...]
set -eux
REPO="$(dirname "$(readlink -f "$0")")/../.."

# A mirror still syncing an update answers 404 for the repodata the index it
# was handed names, and dnf gives up once every mirror has, so the install is
# retried with fresh metadata. apk has no retry of its own.
retry_install() {
    for attempt in 1 2 3 4; do
        "$@" && return 0
        if command -v dnf > /dev/null; then
            dnf clean metadata
        elif command -v yum > /dev/null; then
            yum clean metadata
        fi
        sleep $((attempt * 30))
    done
    return 1
}
if command -v dnf > /dev/null; then
    retry_install dnf install -y --disablerepo='epel*' git nasm bison clang clang-devel \
        pixman-devel libxcb-devel libdrm-devel libinput-devel mesa-libgbm-devel mesa-libEGL-devel \
        wayland-devel systemd-devel
    pulse_libdir=lib64
elif command -v yum > /dev/null; then
    # manylinux2014: CentOS 7, glibc 2.17. Its nasm is older than x264, dav1d,
    # and SVT-AV1 take, and its clang older than bindgen's, so nasm is built
    # from its release, checked against the hash pinned here, and libclang
    # comes from PyPI into the interpreter below.
    retry_install yum install -y --disablerepo='epel*' bison xz pixman-devel libxcb-devel \
        libdrm-devel libinput-devel mesa-libgbm-devel mesa-libEGL-devel wayland-devel \
        systemd-devel
    nasm_version=3.02
    nasm_sha256=87336eba53b4acfe917424ab5d500d2b0054d9f5148d35c2273ccf2cfb712f0d
    curl -fsSL --retry 5 -o /tmp/nasm.tar.xz \
        "https://www.nasm.us/pub/nasm/releasebuilds/${nasm_version}/nasm-${nasm_version}.tar.xz"
    echo "${nasm_sha256}  /tmp/nasm.tar.xz" | sha256sum -c -
    mkdir -p /tmp/nasm
    tar -xJf /tmp/nasm.tar.xz -C /tmp/nasm --strip-components=1
    (cd /tmp/nasm && ./configure --prefix=/usr/local > /dev/null && make -j"$(nproc)" > /dev/null \
        && make install > /dev/null)
    rm -rf /tmp/nasm /tmp/nasm.tar.xz
    set -- libclang "$@"
    centos7=1
    pulse_libdir=lib64
else
    retry_install apk add --no-cache build-base pkgconf linux-headers git curl cmake nasm bison \
        clang pixman-dev libxcb-dev libdrm-dev libinput-dev mesa-dev wayland-dev eudev-dev
    # bindgen opens libclang at run time, and Alpine names that package after
    # the compiler's major version, which moves every release
    retry_install apk add --no-cache "clang$(clang -dumpversion | cut -d. -f1)-libclang"
    pulse_libdir=lib
fi
/opt/python/cp312-cp312/bin/pip install --retries 5 meson ninja "$@"
if [ -n "${centos7:-}" ]; then
    # bindgen finds libclang where clang-sys looks (server.sh hands it GCC's
    # builtin headers, which this libclang carries none of).
    ln -sf "$(/opt/python/cp312-cp312/bin/python -c 'import clang, os; print(os.path.join(os.path.dirname(clang.__file__), "native", "libclang.so"))')" \
        /usr/lib64/libclang.so
fi
# Each distribution's libpulse takes D-Bus and X11 along, and Alpine's
# libsndfile's codecs as well: pcmflux's script builds the current client
# libraries alone in their place.
PATH="/opt/python/cp312-cp312/bin:${PATH}" sh "${REPO}/crates/pcmflux/scripts/build-libpulse.sh" /usr "${pulse_libdir}"
# glibc's loader reads its cache; musl's searches /usr/lib as it is
if [ "${pulse_libdir}" = lib64 ]; then ldconfig; fi
