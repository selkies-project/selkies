#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds XLibre's Xvfb from its release tag with the Selkies patches and
# installs the one binary. One recipe for the base image and the tests
# workflow, so the X server the suites drive is the one the images ship. Only
# the framebuffer server is built, and its paths, extensions and defaults are
# the distribution package's, so the binary drops in where that one installed
# it; anything left at the meson default would move a keymap or a font path
# out from under the rest of the image. The distribution supplies the
# toolchain and libraries; the protocol headers are built from source where
# the installed ones predate what the server asks for.
#
# Inputs: XLIBRE_TAG and XLIBRE_SHA256 (required), the release tag and the
# checksum of the archive GitHub generates for it; PREFIX (default /usr);
# DESTDIR (default empty), the staging root the binary is installed under;
# PATCH_DIR (default: patches/ beside this script).
set -euo pipefail
: "${XLIBRE_TAG:?}"
: "${XLIBRE_SHA256:?}"
PREFIX="${PREFIX:-/usr}"
DESTDIR="${DESTDIR:-}"
PATCH_DIR="${PATCH_DIR:-$(cd "$(dirname "$0")" && pwd)/patches}"
SRC="$(mktemp -d)"
trap 'rm -rf "$SRC"' EXIT
export PKG_CONFIG_PATH="${PREFIX}/share/pkgconfig:${PREFIX}/lib/pkgconfig${PKG_CONFIG_PATH:+:${PKG_CONFIG_PATH}}"

# The protocol headers' tag is for the reader; the commit is what gets built.
XORGPROTO_TAG="xorgproto-2024.1"
XORGPROTO_COMMIT="67469711055522b8adb2d795b01e7ba98cb8816c"
pkg-config --exists 'presentproto >= 1.4' || {
    git clone --depth 1 --branch "${XORGPROTO_TAG}" \
        https://gitlab.freedesktop.org/xorg/proto/xorgproto.git "${SRC}/xorgproto"
    commit="$(git -C "${SRC}/xorgproto" rev-parse HEAD)"
    [ "${commit}" = "${XORGPROTO_COMMIT}" ] || { echo "${XORGPROTO_TAG} is at ${commit}, not ${XORGPROTO_COMMIT}" >&2; exit 1; }
    meson setup "${SRC}/xorgproto/build" "${SRC}/xorgproto" --prefix="${PREFIX}" --libdir=lib
    DESTDIR='' ninja -C "${SRC}/xorgproto/build" install
}

curl -fsSL --retry 5 --connect-timeout 30 -o "${SRC}/xlibre.tar.gz" \
    "https://github.com/X11Libre/xserver/archive/refs/tags/${XLIBRE_TAG}.tar.gz"
echo "${XLIBRE_SHA256}  ${SRC}/xlibre.tar.gz" | sha256sum -c -
tar -xf "${SRC}/xlibre.tar.gz" -C "${SRC}"
cd "${SRC}/xserver-${XLIBRE_TAG}"
for p in "${PATCH_DIR}"/xvfb-*.patch; do
    patch -p1 < "${p}"
done
meson setup build \
    --prefix="${PREFIX}" --sysconfdir=/etc --localstatedir=/var --buildtype=release \
    -Dxvfb=true -Dglamor=true -Dgbm=true -Ddri3=true -Dglx=true \
    -Dxcsecurity=true -Dxselinux=true \
    -Dxorg=false -Dxnest=false -Dxephyr=false -Dxwin=false -Dxquartz=false \
    -Dudev=false -Dudev_kms=false -Dsystemd_logind=false \
    -Dxkb_dir=/usr/share/X11/xkb -Dxkb_output_dir=/var/lib/xkb -Dxkb_bin_dir=/usr/bin \
    -Ddefault_font_path=/usr/share/fonts/X11/misc,/usr/share/fonts/X11/cyrillic,/usr/share/fonts/X11/100dpi/:unscaled,/usr/share/fonts/X11/75dpi/:unscaled,/usr/share/fonts/X11/Type1,/usr/share/fonts/X11/100dpi,/usr/share/fonts/X11/75dpi,built-ins \
    -Ddocs=false -Ddevel-docs=false
ninja -C build hw/vfb/Xvfb
install -Dm755 build/hw/vfb/Xvfb "${DESTDIR}${PREFIX}/bin/Xvfb"
"${DESTDIR}${PREFIX}/bin/Xvfb" -help 2>&1 | grep -q -- '-glamor'
"${DESTDIR}${PREFIX}/bin/Xvfb" -help 2>&1 | grep -q -- 'CRTCs per screen (default: 4)'
