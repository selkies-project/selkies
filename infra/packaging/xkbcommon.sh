#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build libxkbcommon from its pinned release as a static archive under
# xkbcommon/ in SELKIES_TOOLCHAIN (else BUILD) and point the compiler driver and
# pkg-config at it, for a server built where the distribution's libxkbcommon is
# older than the API the server reads keysyms through (Enterprise Linux 8), and
# so that the server links none from the host. Sourced by the bundle build after
# portable-deps.sh, with BUILD set; it exports LIBRARY_PATH and PKG_CONFIG_PATH.
# A finished build there, the one the bundle's toolchain image carries
# (infra/bundle/Dockerfile), is taken as it is. The xkb data stays the host's
# (/usr/share/X11/xkb), which XKB_CONFIG_ROOT overrides where the host has none.

XKB="${SELKIES_TOOLCHAIN:-${BUILD}}/xkbcommon"
if [ ! -f "${XKB}/.complete" ]; then
# libxkbcommon's parser takes bison 3.6 or newer, which CentOS 7 and
# AlmaLinux 8 predate (3.0.4): the release pinned here is built where it is.
# It comes from a GNU mirror through ftpmirror.gnu.org, as GNU asks, and from
# ftp.gnu.org where that fails: CentOS 7's curl retries no failed connection,
# so one unreachable host would fail the build. The pinned digest decides.
bison="$(bison --version | head -n 1 | grep -oE '[0-9]+\.[0-9]+' | head -n 1)"
if [ "$(printf '%s\n3.6\n' "${bison}" | sort -V | head -n 1)" != 3.6 ]; then
    for source in https://ftpmirror.gnu.org/bison https://ftp.gnu.org/gnu/bison; do
        curl -fsSL --retry 5 -o /tmp/bison.tar.xz "${source}/bison-3.8.2.tar.xz" && break
    done
    echo "9bba0214ccf7f1079c5d59210045227bcf619519840ebfa80cd3849cff5a5bf2  /tmp/bison.tar.xz" | sha256sum -c -
    mkdir -p /tmp/bison
    tar -xJf /tmp/bison.tar.xz -C /tmp/bison --strip-components=1
    (cd /tmp/bison && ./configure --prefix=/usr/local > /dev/null && make -j"$(nproc)" > /dev/null \
        && make install > /dev/null)
    rm -rf /tmp/bison /tmp/bison.tar.xz
fi

rm -rf "${XKB}"
git init -q "${XKB}/src"
git -C "${XKB}/src" fetch -q --depth 1 https://github.com/xkbcommon/libxkbcommon.git \
    refs/tags/xkbcommon-1.13.2:refs/tags/xkbcommon-1.13.2
[ "$(git -C "${XKB}/src" rev-parse 'FETCH_HEAD^{commit}')" = d1442aaa6b635551182e83c3b55037f4c118c962 ]
git -C "${XKB}/src" checkout -q FETCH_HEAD
meson setup "${XKB}/build" "${XKB}/src" --prefix="${XKB}" --libdir=lib --buildtype=release \
    --default-library=static -Db_staticpic=true -Denable-x11=false -Denable-wayland=false \
    -Denable-docs=false -Denable-tools=false -Denable-xkbregistry=false \
    -Dxkb-config-root=/usr/share/X11/xkb -Dx-locale-root=/usr/share/X11/locale
ninja -C "${XKB}/build" install
rm -rf "${XKB}/src" "${XKB}/build"
touch "${XKB}/.complete"
fi
# The crates name the library to the linker rather than through pkg-config, so
# its directory goes ahead of the system's for the compiler driver as well
export LIBRARY_PATH="${XKB}/lib${LIBRARY_PATH:+:${LIBRARY_PATH}}" PKG_CONFIG_PATH="${XKB}/lib/pkgconfig${PKG_CONFIG_PATH:+:${PKG_CONFIG_PATH}}"
