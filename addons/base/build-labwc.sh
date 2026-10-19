#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the pinned wlroots and the Selkies-patched labwc into PREFIX. One
# recipe for the base image's labwc stage and the tests workflow, so the
# compositor the suites drive is the one the images ship. The distribution
# supplies the toolchain and libraries; the dependencies the pinned wlroots
# may need newer than a distribution carries (wayland, wayland-protocols,
# pixman, libliftoff, libdisplay-info) are built from source only where the
# installed ones are too old, and the result finds them through its rpath.
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

# The commit each tag has to resolve to: the tag is for the reader, the commit
# is what gets built, and a version with none recorded here is refused.
declare -A COMMITS=(
    [wayland@1.23.1]=a9fec8dd65977c57f4039ced34327204d9b9d779
    [wayland-protocols@1.41]=71da8bd7f9fd3ea7c5d87ad19ca087eeca137673
    [pixman@pixman-0.43.4]=54cad71674ec485cbbbf49876feaa8a69b97c828
    [libliftoff@v0.5.0]=8b08dc1c14fd019cc90ddabe34ad16596b0691f4
    [libdisplay-info@0.2.0]=66b802d05b374cd8f388dc6ad1e7ae4f08cb3300
    [wlroots@0.19.3]=88a869855742281c98c22cab9641b317b8d065ef
    [labwc@0.9.7]=f0dbad27fcf6e388cad1aa32448a1d548da68990
)

# The forges refuse a clone now and then (git exits 128); a few tries some
# seconds apart ride over it.
fetch() {
    local url="$1" ref="$2" dir="$3" try commit
    local expected="${COMMITS[${dir##*/}@${ref}]:-}"
    [ -n "${expected}" ] || { echo "no commit recorded for ${dir##*/} ${ref}" >&2; return 1; }
    for try in 1 2 3 4 5; do
        if git clone --depth 1 --branch "${ref}" "${url}" "${dir}"; then
            commit="$(git -C "${dir}" rev-parse HEAD)"
            [ "${commit}" = "${expected}" ] && return 0
            echo "${dir##*/} ${ref} is at ${commit}, not ${expected}" >&2
            return 1
        fi
        rm -rf "${dir}"
        sleep $((try * 15))
    done
    return 1
}

build() {
    local name="$1" url="$2" ref="$3"
    shift 3
    fetch "${url}" "${ref}" "${SRC}/${name}"
    meson setup "${SRC}/${name}/build" "${SRC}/${name}" \
        --prefix="${PREFIX}" --libdir=lib --buildtype=release "$@"
    ninja -C "${SRC}/${name}/build"
    ninja -C "${SRC}/${name}/build" install
}

pkg-config --exists 'wayland-server >= 1.23.1' || \
    build wayland https://gitlab.freedesktop.org/wayland/wayland.git 1.23.1 \
        -Ddocumentation=false -Dtests=false -Ddtd_validation=false
pkg-config --exists 'wayland-protocols >= 1.41' || \
    build wayland-protocols https://gitlab.freedesktop.org/wayland/wayland-protocols.git 1.41 \
        -Dtests=false
pkg-config --exists 'pixman-1 >= 0.43.0' || \
    build pixman https://gitlab.freedesktop.org/pixman/pixman.git pixman-0.43.4 \
        -Dtests=disabled -Ddemos=disabled -Dgtk=disabled
pkg-config --exists 'libliftoff >= 0.5.0' || \
    build libliftoff https://gitlab.freedesktop.org/emersion/libliftoff.git v0.5.0
pkg-config --exists 'libdisplay-info >= 0.2.0' || \
    build libdisplay-info https://gitlab.freedesktop.org/emersion/libdisplay-info.git 0.2.0

build wlroots https://gitlab.freedesktop.org/wlroots/wlroots.git "${WLROOTS_VERSION}" \
    -Dxwayland=enabled

fetch https://github.com/labwc/labwc.git "${LABWC_VERSION}" "${SRC}/labwc"
git -C "${SRC}/labwc" apply "${PATCH_DIR}/labwc-ipc.patch" "${PATCH_DIR}/labwc-screens.patch"
meson setup "${SRC}/labwc/build" "${SRC}/labwc" \
    --prefix="${PREFIX}" --libdir=lib --buildtype=release \
    -Dxwayland=enabled -Dnls=enabled
ninja -C "${SRC}/labwc/build"
ninja -C "${SRC}/labwc/build" install
"${PREFIX}/bin/labwc" --version
