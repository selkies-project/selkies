#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Stage the Joystick Interposer into a package root, so every native package
# carries it. Unlike fake-udev it shadows nothing: the library is inert until an
# application is started with it preloaded.
#
# Usage: interposer.sh <pkg-root>
set -eu
PKG_ROOT="${1:?usage: interposer.sh <pkg-root>}"
SRC="$(dirname "$(readlink -f "$0")")/../../addons/js-interposer/joystick_interposer.c"
CC="${CC:-gcc}"

# `/usr/$LIB`, which is how the interposer is preloaded, expands at load time to
# the multiarch triplet on Debian-family systems and to lib64 elsewhere on
# 64-bit (Arch symlinks lib64 onto lib, and musl has neither).
MULTIARCH="$("${CC}" -print-multiarch 2>/dev/null || true)"
if [ -n "${MULTIARCH}" ]; then
    LIBDIR="usr/lib/${MULTIARCH}"
elif [ -d /usr/lib64 ] && [ ! -L /usr/lib64 ]; then
    LIBDIR="usr/lib64"
else
    LIBDIR="usr/lib"
fi
mkdir -p "${PKG_ROOT}/${LIBDIR}"
"${CC}" -O2 -shared -fPIC -o "${PKG_ROOT}/${LIBDIR}/selkies_joystick_interposer.so" \
    "${SRC}" -ldl -pthread
echo "interposer: /${LIBDIR}/selkies_joystick_interposer.so"

# The 32-bit variant serves the Wine and Steam catalog, which `/usr/$LIB`
# resolves to per process bitness. It needs a multilib toolchain, which only
# some of the target distributions offer, so its absence is reported rather
# than fatal.
if ! echo 'int main(void){return 0;}' | "${CC}" -m32 -x c - -o /dev/null 2>/dev/null; then
    echo "interposer: no 32-bit toolchain here, skipping the 32-bit variant" >&2
    exit 0
fi
MULTIARCH32="$("${CC}" -m32 -print-multiarch 2>/dev/null || true)"
if [ -n "${MULTIARCH32}" ]; then
    LIB32DIR="usr/lib/$(echo "${MULTIARCH32}" | sed -e 's/i.*86/i386/')"
else
    LIB32DIR="usr/lib"
fi
mkdir -p "${PKG_ROOT}/${LIB32DIR}"
"${CC}" -m32 -O2 -shared -fPIC -o "${PKG_ROOT}/${LIB32DIR}/selkies_joystick_interposer.so" \
    "${SRC}" -ldl -pthread
echo "interposer: /${LIB32DIR}/selkies_joystick_interposer.so"
