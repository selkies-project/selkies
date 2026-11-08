#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install the native package in /out and prove what it lays down works with
# nothing but the package's own declared dependencies.
#
# The build container carries a whole toolchain, so a runtime library the
# package forgot to declare is satisfied there and missing on a user's machine.
# This runs in a pristine image of the same distribution instead, and checks
# what a bare `--help` does not: that every library the server and the
# interposers link resolves, that the libraries pixelflux and pcmflux open by name
# at runtime (no ELF dependency names them) are present, and that the server
# finds its web client where it looks for it.
#
# $INSTALL is the distribution's install command for /out/*, from the packages
# workflow matrix.

set -eu

sh -c "${INSTALL}" > /dev/null

selkies --version
selkies --help > /dev/null
echo "selkies --version and --help run"

test -f /usr/share/selkies/web/index.html || {
    echo "::error::no web client at /usr/share/selkies/web"
    exit 1
}
echo "the web client is installed beside the server"

interposers="$(find /usr/lib /usr/lib64 -name 'selkies_*_interposer.so' 2>/dev/null || true)"
[ -n "${interposers}" ] || {
    echo "::error::the package installed no interposers"
    exit 1
}
missing="$(for f in /usr/bin/selkies ${interposers}; do
    ldd "${f}" 2>/dev/null | grep 'not found' | sed "s|^|${f}: |"
done | sort -u)"
if [ -n "${missing}" ]; then
    echo "::error::installed files with unresolved dependencies:"
    echo "${missing}"
    exit 1
fi
echo "the server and the interposers resolve every library they link"

# Opened by name at runtime: pixelflux reaches for the Wayland server library,
# VA-API, and the GPU stack, so no ELF dependency names them and only a
# declared package dependency puts them on the system.
NEEDED="libwayland-server.so.0 libEGL.so.1 libgbm.so.1 libpulse.so.0 libva.so.2 libva-drm.so.2 libdrm.so.2 libxkbcommon.so.0 libpixman-1.so.0"
absent=""
for name in ${NEEDED}; do
    found=""
    for dir in /lib /lib64 /usr/lib /usr/lib64 /usr/lib/*-linux-gnu /lib/*-linux-gnu; do
        [ -e "${dir}/${name}" ] && found=1 && break
    done
    [ -n "${found}" ] || absent="${absent} ${name}"
done
if [ -n "${absent}" ]; then
    echo "::error::runtime-loaded libraries the package does not pull in:${absent}"
    exit 1
fi
echo "every runtime-loaded library is present"

# Arch installs the CUPS scheduler readable by root alone; the package's pacman
# hook opens it on the cups install, which is what lets the print queue run a
# copy of it as the session user.
if command -v pacman > /dev/null; then
    pacman -S --noconfirm --needed cups > /dev/null
    mode="$(stat -c %a /usr/bin/cupsd)"
    [ "${mode}" = "755" ] || {
        echo "::error::the pacman hook left /usr/bin/cupsd at mode ${mode}"
        exit 1
    }
    echo "the pacman hook opened the CUPS scheduler"
fi
