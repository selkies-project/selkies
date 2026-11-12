#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the bundle's CUPS scheduler into <tree>/libexec/selkies/cups, laid out
# as an installed CUPS is (sbin/cupsd, lib/cups/daemon and lib/cups/filter,
# share/cups), which the print queue (crates/selkies/src/files/printing.rs)
# finds beside its executable and runs as the session user under the runtime
# directory. Only the scheduler and the programs it runs are installed, from the
# pinned release, with libcups linked in and nothing a session's queue does not
# use (TLS, DNS-SD, D-Bus, PAM, USB, the web interface): the queue takes the
# PDF that browsers and the desktop toolkits print as it is, so cups-filters
# and Ghostscript stay out, and a document of another format is refused rather
# than converted.
#
# Runs inside the bundle's build image after xservers.sh, whose static zlib it
# links. Its source is fetched once into SELKIES_SOURCE_CACHE.
#
# Usage: cups.sh <tree>
set -eux
TREE="$(readlink -f "${1:?usage: cups.sh <tree>}")"
BUILD="${SELKIES_BUILD_DIR:-/build}/cups"
CACHE="${SELKIES_SOURCE_CACHE:-${BUILD}/sources}"
DEPS="${SELKIES_XSERVERS_DIR:-${SELKIES_BUILD_DIR:-/build}/xservers}/deps"
VERSION=2.4.20
SHA256=ab4d9cd7f3e58060091d2b24972223d6401675f11c49b65abf4f6ef31dea22ff
OUT="${TREE}/libexec/selkies/cups"
mkdir -p "${BUILD}" "${CACHE}"

name="cups-${VERSION}-source.tar.gz"
if [ ! -f "${CACHE}/${name}" ]; then
    curl -fsSL --retry 5 --connect-timeout 30 -o "${CACHE}/${name}.part" \
        "https://github.com/OpenPrinting/cups/releases/download/v${VERSION}/${name}"
    mv "${CACHE}/${name}.part" "${CACHE}/${name}"
fi
echo "${SHA256}  ${CACHE}/${name}" | sha256sum -c -
rm -rf "${BUILD}/src" "${BUILD}/stage"
mkdir -p "${BUILD}/src"
tar -xzf "${CACHE}/${name}" -C "${BUILD}/src" --strip-components=1
# A build without DNS-SD lacks the local-queue fields the enumeration fills
patch -d "${BUILD}/src" -p1 < "$(dirname "$(readlink -f "$0")")/patches/cups/without-dnssd.patch"
(cd "${BUILD}/src" && CFLAGS="-O2 -fPIC" CXXFLAGS="-O2 -fPIC" CPPFLAGS="-I${DEPS}/include" LDFLAGS="-L${DEPS}/lib" ./configure \
    --prefix=/usr --sysconfdir=/etc --localstatedir=/var --libdir=/usr/lib \
    --disable-shared --enable-static --with-tls=no --with-dnssd=no \
    --disable-dbus --disable-pam --disable-libusb --disable-gssapi --disable-libpaper \
    --disable-acl --disable-webif --disable-tcp-wrappers --without-systemd --with-ondemand=no \
    --with-languages=none > /dev/null)
make -C "${BUILD}/src" -j"$(nproc)" > /dev/null
make -C "${BUILD}/src" DESTDIR="${BUILD}/stage" install > /dev/null

# The scheduler, the helper it runs every filter and backend through, and the
# types it reads a document's format from: the queue's backend is its own, and
# it converts nothing, so no filter, backend, or tool of CUPS's is carried
rm -rf "${OUT}"
mkdir -p "${OUT}/sbin" "${OUT}/lib/cups/daemon" "${OUT}/lib/cups/filter" "${OUT}/share/cups"
install -m755 "${BUILD}/stage/usr/sbin/cupsd" "${OUT}/sbin/cupsd"
install -m755 "${BUILD}/stage/usr/lib/cups/daemon/cups-exec" "${OUT}/lib/cups/daemon/cups-exec"
cp -a "${BUILD}/stage/usr/share/cups/mime" "${OUT}/share/cups/mime"
test -x "${OUT}/lib/cups/daemon/cups-exec"
test -d "${OUT}/lib/cups/filter"
test -f "${OUT}/share/cups/mime/mime.types"
readelf -d "${OUT}/sbin/cupsd" | grep NEEDED
