#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install everything the bundle's build takes from its manylinux or
# musllinux_1_2 image rather than from this tree: what the server's build needs
# (infra/packaging/portable-deps.sh, PulseAudio's client libraries among it),
# the PulseAudio server of that same release, the tools and headers the X
# servers, NVIDIA's X11 EGL platform, dbus-daemon, and libxcrypt build against
# (Perl's open module, which AlmaLinux 8 packages apart, for libxcrypt), and, on
# glibc, a 32-bit toolchain for the interposers.
# infra/bundle/Dockerfile runs it once into the toolchain image the build then
# runs in; build-in-image.sh runs it itself in a bare manylinux image.
#
# manylinux2014 (CentOS 7) has two of those too old: its pkg-config (0.27)
# follows the cycle between the gl.pc xservers.sh writes and libepoxy's epoxy.pc
# until it runs out of files, so pkgconf is built from its release in its place,
# and its kernel headers (3.10) lack interfaces the bundle includes or tests
# for: Xwayland's DMA-BUF and sync file ioctls, the input interposer's event
# codes and uinput device setup, and the v4l2 interposer's V4L2 requests and
# statx. Those headers come from AlmaLinux 8's kernel-headers package, the
# version the manylinux_2_28 image infra/bundle/Dockerfile pins carries, so both
# architectures build against the same kernel interfaces. So do gbm.h and
# gbm.pc, from that image's mesa-libgbm-devel: the X servers reach libgbm only
# through the loader xservers.sh builds, and configure the calls it forwards
# (gbm_bo_get_fd_for_plane, gbm_bo_create_with_modifiers2, front rendering) by
# that version, which Mesa 18.3's would leave out. Each download is checked
# against the hash pinned here.
#
# Usage: deps.sh
set -eux
REPO="$(dirname "$(readlink -f "$0")")/../.."

"${REPO}/infra/packaging/portable-deps.sh"
# Where the image's distribution puts libraries: Enterprise Linux's lib64,
# Alpine's lib
libdir=lib64
[ -d /usr/lib64 ] && [ ! -L /usr/lib64 ] || libdir=lib
PATH="/opt/python/cp312-cp312/bin:${PATH}" sh "${REPO}/crates/pcmflux/scripts/build-libpulse.sh" \
    /usr "${libdir}" daemon
if command -v dnf > /dev/null; then
    dnf install -y --disablerepo='epel*' bison flex expat-devel mesa-libgbm-devel \
        mesa-libEGL-devel mesa-libGL-devel libX11-devel libxslt gettext perl-XML-Parser xz \
        perl-open
    dnf install -y --disablerepo='epel*' glibc-devel.i686 libgcc.i686 || true
    dnf clean all
    ldconfig
elif command -v yum > /dev/null; then
    yum install -y --disablerepo='epel*' --disablerepo='centos-sclo*' bison flex expat-devel \
        mesa-libgbm-devel mesa-libEGL-devel mesa-libGL-devel libX11-devel libxslt gettext \
        perl-XML-Parser xz
    yum install -y --disablerepo='epel*' --disablerepo='centos-sclo*' glibc-devel.i686 \
        libgcc.i686 || true
    yum clean all
    ldconfig
    curl -fsSL --retry 5 -o /tmp/pkgconf.tar.gz \
        https://github.com/pkgconf/pkgconf/archive/refs/tags/pkgconf-3.0.10.tar.gz
    echo "11818a25f2730132f3b258ac8be4e82f83b9db03a1a99a6c29cd01b94fc9d310  /tmp/pkgconf.tar.gz" \
        | sha256sum -c -
    mkdir -p /tmp/pkgconf
    tar -xzf /tmp/pkgconf.tar.gz -C /tmp/pkgconf --strip-components=1
    PATH="/opt/python/cp312-cp312/bin:${PATH}" meson setup /tmp/pkgconf/build /tmp/pkgconf \
        --prefix=/usr/local --buildtype=release --default-library=static \
        -Dwith-pkg-config-dir=/usr/lib64/pkgconfig:/usr/share/pkgconfig \
        -Dwith-system-libdir=/usr/lib64:/usr/lib -Dwith-system-includedir=/usr/include
    PATH="/opt/python/cp312-cp312/bin:${PATH}" ninja -C /tmp/pkgconf/build install > /dev/null
    ln -sf pkgconf /usr/local/bin/pkg-config
    rm -rf /tmp/pkgconf /tmp/pkgconf.tar.gz
    kernel_headers=kernel-headers-4.18.0-553.186.1.el8_10.x86_64.rpm
    for source in https://repo.almalinux.org/almalinux/8.10/BaseOS/x86_64/os/Packages \
            https://vault.almalinux.org/8.10/BaseOS/x86_64/os/Packages; do
        curl -fsSL --retry 5 -o "/tmp/${kernel_headers}" "${source}/${kernel_headers}" && break
    done
    echo "b5b8d25385a522daf91454650a326cb25d591d2b3522c038489e66412572b78d  /tmp/${kernel_headers}" \
        | sha256sum -c -
    (cd / && rpm2cpio "/tmp/${kernel_headers}" | cpio -idmu --quiet \
        ./usr/include/linux/dma-buf.h ./usr/include/linux/sync_file.h \
        ./usr/include/linux/input.h ./usr/include/linux/input-event-codes.h ./usr/include/linux/uinput.h \
        ./usr/include/linux/videodev2.h ./usr/include/linux/v4l2-common.h \
        ./usr/include/linux/v4l2-controls.h ./usr/include/linux/stat.h)
    grep -q UI_DEV_SETUP /usr/include/linux/uinput.h
    rm -f "/tmp/${kernel_headers}"
    gbm_devel=mesa-libgbm-devel-23.1.4-4.el8_10.alma.1.x86_64.rpm
    for source in https://repo.almalinux.org/almalinux/8.10/PowerTools/x86_64/os/Packages \
            https://vault.almalinux.org/8.10/PowerTools/x86_64/os/Packages; do
        curl -fsSL --retry 5 -o "/tmp/${gbm_devel}" "${source}/${gbm_devel}" && break
    done
    echo "ad2cada9d87de59390c107c9e0f8c1ce0def956dc77f0604ae3f12d17087fcd5  /tmp/${gbm_devel}" \
        | sha256sum -c -
    (cd / && rpm2cpio "/tmp/${gbm_devel}" | cpio -idmu --quiet ./usr/include/gbm.h \
        ./usr/lib64/pkgconfig/gbm.pc)
    [ "$(pkg-config --modversion gbm)" = 23.1.4 ]
    rm -f "/tmp/${gbm_devel}"
else
    apk add --no-cache bison flex expat-dev mesa-dev libxslt gettext perl-xml-parser xz \
        linux-headers patch
fi
