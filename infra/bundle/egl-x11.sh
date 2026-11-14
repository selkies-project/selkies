#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build NVIDIA's EGL platform library for X11 (xcb and xlib) into <prefix>, at
# the release pinned here: through it a driver's EGL reaches X11 windows on the
# framebuffer server's DRI3. It is open source and independent of the driver's
# version, and the glibc bundle carries it for the images (addons/base), whose
# runtimes inject a driver without it, which install it where that driver's EGL
# looks; Ubuntu packages an older release and Debian none. A host running the
# bundle has it from its own driver installation, so nothing there loads the
# copy. It links the X and graphics libraries every such host has, as a
# distribution's build does, and is built against the eglexternalplatform
# interface it needs, pinned here too, which AlmaLinux 8's predates. Where the
# image's EGL headers are not libglvnd's (CentOS 7's are Mesa 18.3's, which
# predate the EGL 1.5 function types it declares), it takes those of the
# libglvnd release pinned here, and where the image's libdrm headers predate the
# formats it lists (CentOS 7's 2.4.97), those of the libdrm release pinned here,
# still linking the image's libdrm, whose functions it calls are all older.
#
# Runs in the bundle's glibc build image after infra/bundle/deps.sh, once into
# the toolchain image (infra/bundle/Dockerfile); a <prefix> holding a finished
# build is left as it is.
#
# Usage: egl-x11.sh <prefix>
#   writes <prefix>/lib/libnvidia-egl-{xcb,xlib}.so.1 and
#   <prefix>/share/egl/egl_external_platform.d/20_nvidia_{xcb,xlib}.json
set -eux
PREFIX="$(mkdir -p "${1:?usage: egl-x11.sh <prefix>}" && readlink -f "$1")"
[ ! -f "${PREFIX}/.complete" ] || exit 0
EGL_X11_TAG="v1.0.6"
EGL_X11_SHA256="370870b0dea12dde0ddac0b77594ff94215cf2694ac0a5ded98f5d2f9864f6e0"
PLATFORM_TAG="1.2.1"
PLATFORM_SHA256="5089ceb054ca50c85837f015756a3d0f2f75cf2a98c9e5fbcbcfb8206137f76e"
GLVND_TAG="v1.7.0"
GLVND_SHA256="2b6e15b06aafb4c0b6e2348124808cbd9b291c647299eaaba2e3202f51ff2f3d"
LIBDRM_VERSION="2.4.125"
LIBDRM_SHA256="d4bae92797a50f81a93524762e0410a49cd84cfa0f997795bc0172ac8fb1d96a"
WORK="$(mktemp -d)"
export PATH="/opt/python/cp312-cp312/bin:${PATH}"
export PKG_CONFIG_PATH="${WORK}/platform/share/pkgconfig${PKG_CONFIG_PATH:+:${PKG_CONFIG_PATH}}"

# fetch <url> <sha256> <dir>: the archive checked and unpacked into <dir>
fetch() {
    curl -fsSL --retry 8 --retry-delay 30 --connect-timeout 30 -o "${WORK}/archive.tar.gz" "$1"
    echo "$2  ${WORK}/archive.tar.gz" | sha256sum -c -
    mkdir -p "$3"
    tar -xf "${WORK}/archive.tar.gz" -C "$3" --strip-components=1
}

# The EGL API version libglvnd's egl.pc carries, which egl-x11's build requires
if ! pkg-config --exists 'egl >= 1.5' || ! pkg-config --exists 'egl < 2'; then
    fetch "https://gitlab.freedesktop.org/glvnd/libglvnd/-/archive/${GLVND_TAG}/libglvnd-${GLVND_TAG}.tar.gz" \
        "${GLVND_SHA256}" "${WORK}/glvnd"
    mkdir -p "${WORK}/headers/include" "${WORK}/headers/pkgconfig"
    cp -r "${WORK}/glvnd/include/EGL" "${WORK}/glvnd/include/KHR" "${WORK}/headers/include/"
    printf 'Name: egl\nDescription: EGL headers\nVersion: 1.5\nCflags: -I%s\n' \
        "${WORK}/headers/include" > "${WORK}/headers/pkgconfig/egl.pc"
    export PKG_CONFIG_PATH="${WORK}/headers/pkgconfig:${PKG_CONFIG_PATH}"
fi
drm_cflags="$(pkg-config --cflags libdrm)"
# shellcheck disable=SC2086  # drm_cflags is a list of flags
if ! printf '#include <drm_fourcc.h>\nint format = DRM_FORMAT_ABGR16161616F;\n' \
        | cc ${drm_cflags} -x c -c -o /dev/null - 2> /dev/null; then
    fetch "https://dri.freedesktop.org/libdrm/libdrm-${LIBDRM_VERSION}.tar.xz" "${LIBDRM_SHA256}" \
        "${WORK}/libdrm"
    mkdir -p "${WORK}/headers/include/libdrm" "${WORK}/headers/pkgconfig"
    cp "${WORK}"/libdrm/include/drm/*.h "${WORK}/headers/include/libdrm/"
    cp "${WORK}/libdrm/xf86drm.h" "${WORK}/libdrm/xf86drmMode.h" "${WORK}/headers/include/"
    printf 'Name: libdrm\nDescription: libdrm headers\nVersion: %s\nCflags: -I%s -I%s/libdrm\nLibs: -ldrm\n' \
        "${LIBDRM_VERSION}" "${WORK}/headers/include" "${WORK}/headers/include" \
        > "${WORK}/headers/pkgconfig/libdrm.pc"
    export PKG_CONFIG_PATH="${WORK}/headers/pkgconfig:${PKG_CONFIG_PATH}"
fi

fetch "https://github.com/NVIDIA/eglexternalplatform/archive/refs/tags/${PLATFORM_TAG}.tar.gz" \
    "${PLATFORM_SHA256}" "${WORK}/platform-src"
meson setup "${WORK}/platform-build" "${WORK}/platform-src" --prefix="${WORK}/platform"
ninja -C "${WORK}/platform-build" install

fetch "https://github.com/NVIDIA/egl-x11/archive/refs/tags/${EGL_X11_TAG}.tar.gz" \
    "${EGL_X11_SHA256}" "${WORK}/egl-x11"
meson setup "${WORK}/egl-x11/build" "${WORK}/egl-x11" --prefix=/usr --libdir=lib --buildtype=release
DESTDIR="${WORK}/stage" ninja -C "${WORK}/egl-x11/build" install
mkdir -p "${PREFIX}/lib" "${PREFIX}/share/egl/egl_external_platform.d"
for platform in xcb xlib; do
    cp -L "${WORK}/stage/usr/lib/libnvidia-egl-${platform}.so.1" "${PREFIX}/lib/"
    cp "${WORK}/stage/usr/share/egl/egl_external_platform.d/20_nvidia_${platform}.json" \
        "${PREFIX}/share/egl/egl_external_platform.d/"
done
rm -rf "${WORK}"
touch "${PREFIX}/.complete"
