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
# The print queue runs a copy of cupsd outside the bundle, which its run path
# does not reach, so the libcrypt it links has to be one hosts carry,
# libcrypt.so.1. Where the image's link name is another (manylinux2014's
# libxcrypt is libcrypt.so.2, its libcrypt.so.1 exporting crypt only at glibc's
# own symbol version), crypt is bound to that version of libcrypt.so.1, which
# glibc's libcrypt and libxcrypt's compatible builds export alike.
#
# Runs inside the bundle's build image after xservers.sh, whose static zlib it
# links, once into the bundle's toolchain image (infra/bundle/Dockerfile) under
# SELKIES_TOOLCHAIN: a build that finds the staged install there finished takes
# the scheduler from it. Its source is fetched once into SELKIES_SOURCE_CACHE.
#
# Usage: cups.sh <tree>
set -eux
TREE="$(readlink -f "${1:?usage: cups.sh <tree>}")"
TOOLCHAIN="${SELKIES_TOOLCHAIN:-${SELKIES_BUILD_DIR:-/build}}"
BUILD="${TOOLCHAIN}/cups"
CACHE="${SELKIES_SOURCE_CACHE:-${BUILD}/sources}"
DEPS="${TOOLCHAIN}/xservers/deps"
VERSION=2.4.20
SHA256=ab4d9cd7f3e58060091d2b24972223d6401675f11c49b65abf4f6ef31dea22ff
OUT="${TREE}/libexec/selkies/cups"
mkdir -p "${BUILD}" "${CACHE}"

if [ ! -f "${BUILD}/stage/.complete" ]; then
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
CRYPT_LIBS=""
# The file the linker takes for -lcrypt
libcrypt="$(printf 'int main(void) { return 0; }\n' | cc -x c -o /dev/null -Wl,--trace -lcrypt - 2> /dev/null \
    | grep -o '/[^ ()]*/libcrypt\.[^ ()]*' | head -n 1 || true)"
if [ -n "${libcrypt}" ] && [ "$(objdump -p "${libcrypt}" | awk '$1 == "SONAME" {print $2}')" != libcrypt.so.1 ] \
        && [ -e "$(dirname "${libcrypt}")/libcrypt.so.1" ]; then
    libcrypt="$(dirname "${libcrypt}")/libcrypt.so.1"
    version="$(objdump -T "${libcrypt}" | awk '$NF == "crypt" {gsub(/[()]/, "", $(NF - 1)); print $(NF - 1)}')"
    printf 'char *crypt_versioned(const char *, const char *);\n__asm__(".symver crypt_versioned,crypt@%s");\nchar *crypt(const char *key, const char *salt) { return crypt_versioned(key, salt); }\n' \
        "${version}" > "${BUILD}/crypt.c"
    cc -O2 -fPIC -c "${BUILD}/crypt.c" -o "${BUILD}/crypt.o"
    rm -f "${BUILD}/libcrypt-versioned.a"
    ar rcs "${BUILD}/libcrypt-versioned.a" "${BUILD}/crypt.o"
    CRYPT_LIBS="${BUILD}/libcrypt-versioned.a ${libcrypt}"
fi
(cd "${BUILD}/src" && CFLAGS="-O2 -fPIC" CXXFLAGS="-O2 -fPIC" CPPFLAGS="-I${DEPS}/include" LDFLAGS="-L${DEPS}/lib" LIBS="${CRYPT_LIBS}" ./configure \
    --prefix=/usr --sysconfdir=/etc --localstatedir=/var --libdir=/usr/lib \
    --disable-shared --enable-static --with-tls=no --with-dnssd=no \
    --disable-dbus --disable-pam --disable-libusb --disable-gssapi --disable-libpaper \
    --disable-acl --disable-webif --disable-tcp-wrappers --without-systemd --with-ondemand=no \
    --with-languages=none > /dev/null)
make -C "${BUILD}/src" -j"$(nproc)" > /dev/null
make -C "${BUILD}/src" DESTDIR="${BUILD}/stage" install > /dev/null
rm -rf "${BUILD}/src"
touch "${BUILD}/stage/.complete"
fi

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
