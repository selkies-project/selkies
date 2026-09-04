#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the pinned wlroots and the Selkies-patched labwc into PREFIX. One
# recipe for the base image's labwc stage and the tests workflow, so the
# compositor the suites drive is the one the images ship. The distribution
# supplies the toolchain and libraries; the dependencies the pinned wlroots
# may need newer than a distribution carries (wayland, wayland-protocols,
# libdrm, xkbcommon, pixman, libinput, libliftoff, libdisplay-info) are built from
# source only where the installed ones are too old, at the versions the
# current Ubuntu ships, and the result finds them through its rpath.
#
# Inputs: WLROOTS_VERSION and LABWC_VERSION (required); PREFIX (default
# /usr); PATCH_DIR (default: patches/ beside this script).
set -euo pipefail
: "${WLROOTS_VERSION:?}" "${LABWC_VERSION:?}"
PREFIX="${PREFIX:-/usr}"
PATCH_DIR="${PATCH_DIR:-$(cd "$(dirname "$0")" && pwd)/patches}"
SRC="$(mktemp -d)"
trap 'rm -rf "$SRC"' EXIT
# wayland-protocols installs its pkg-config file under share, the libraries under lib.
export PKG_CONFIG_PATH="${PREFIX}/lib/pkgconfig:${PREFIX}/share/pkgconfig${PKG_CONFIG_PATH:+:${PKG_CONFIG_PATH}}"
# Everything built here finds its libraries through its own rpath, so running
# the result needs no LD_LIBRARY_PATH -- an environment that would leak into
# every other process a CI job runs.
export LDFLAGS="-Wl,-rpath,${PREFIX}/lib${LDFLAGS:+ ${LDFLAGS}}"

build() {
    local name="$1" url="$2" ref="$3"
    shift 3
    git clone --depth 1 --branch "${ref}" "${url}" "${SRC}/${name}"
    meson setup "${SRC}/${name}/build" "${SRC}/${name}" \
        --prefix="${PREFIX}" --libdir=lib --buildtype=release "$@"
    ninja -C "${SRC}/${name}/build"
    ninja -C "${SRC}/${name}/build" install
}

pkg-config --exists 'wayland-server >= 1.24.0' || \
    build wayland https://gitlab.freedesktop.org/wayland/wayland.git 1.24.0 \
        -Ddocumentation=false -Dtests=false -Ddtd_validation=false
pkg-config --exists 'wayland-protocols >= 1.47' || \
    build wayland-protocols https://gitlab.freedesktop.org/wayland/wayland-protocols.git 1.47 \
        -Dtests=false
pkg-config --exists 'libdrm >= 2.4.129' || \
    build libdrm https://gitlab.freedesktop.org/mesa/drm.git libdrm-2.4.131 \
        -Dtests=false -Dman-pages=disabled
pkg-config --exists 'xkbcommon >= 1.8.0' || \
    build xkbcommon https://github.com/xkbcommon/libxkbcommon.git xkbcommon-1.13.1 \
        -Denable-docs=false -Denable-tools=false -Denable-x11=false -Denable-wayland=false
pkg-config --exists 'pixman-1 >= 0.46.0' || \
    build pixman https://gitlab.freedesktop.org/pixman/pixman.git pixman-0.46.4 \
        -Dtests=disabled -Ddemos=disabled -Dgtk=disabled
pkg-config --exists 'libinput >= 1.26' || \
    build libinput https://gitlab.freedesktop.org/libinput/libinput.git 1.31.1 \
        -Dtests=false -Ddocumentation=false -Ddebug-gui=false -Dlibwacom=false
pkg-config --exists 'libliftoff >= 0.5.0' || \
    build libliftoff https://gitlab.freedesktop.org/emersion/libliftoff.git v0.5.0
pkg-config --exists 'libdisplay-info >= 0.2.0' || \
    build libdisplay-info https://gitlab.freedesktop.org/emersion/libdisplay-info.git 0.2.0

build wlroots https://gitlab.freedesktop.org/wlroots/wlroots.git "${WLROOTS_VERSION}" \
    -Dxwayland=enabled

git clone --depth 1 --branch "${LABWC_VERSION}" \
    https://github.com/labwc/labwc.git "${SRC}/labwc"
git -C "${SRC}/labwc" apply "${PATCH_DIR}/labwc-ipc.patch" \
    "${PATCH_DIR}/labwc-seam.patch" "${PATCH_DIR}/labwc-screens.patch"
meson setup "${SRC}/labwc/build" "${SRC}/labwc" \
    --prefix="${PREFIX}" --libdir=lib --buildtype=release \
    -Dxwayland=enabled -Dnls=enabled
ninja -C "${SRC}/labwc/build"
ninja -C "${SRC}/labwc/build" install
"${PREFIX}/bin/labwc" --version
