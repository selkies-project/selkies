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
# On glibc the scheduler links libxcrypt's libcrypt.so.1, built from the release
# pinned here with glibc's interface and put in the tree's lib/selkies, rather
# than the builder image's libcrypt. The print queue runs a copy of cupsd
# outside the bundle, which its run path does not reach, so crypt is bound to
# the symbol version libcrypt.so.1 keeps for glibc compatibility, which glibc's
# own libcrypt and libxcrypt's compatible builds export alike: the copy runs
# against whichever libcrypt.so.1 a host has.
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
XCRYPT_VERSION=4.5.2
XCRYPT_SHA256=71513a31c01a428bccd5367a32fd95f115d6dac50fb5b60c779d5c7942aec071
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
CRYPT_CPPFLAGS=""
if ! ldd --version 2>&1 | grep -qi musl; then
    xcrypt="libxcrypt-${XCRYPT_VERSION}.tar.xz"
    if [ ! -f "${CACHE}/${xcrypt}" ]; then
        curl -fsSL --retry 5 --connect-timeout 30 -o "${CACHE}/${xcrypt}.part" \
            "https://github.com/besser82/libxcrypt/releases/download/v${XCRYPT_VERSION}/${xcrypt}"
        mv "${CACHE}/${xcrypt}.part" "${CACHE}/${xcrypt}"
    fi
    echo "${XCRYPT_SHA256}  ${CACHE}/${xcrypt}" | sha256sum -c -
    rm -rf "${BUILD}/xcrypt-src" "${BUILD}/xcrypt"
    mkdir -p "${BUILD}/xcrypt-src"
    tar -xJf "${CACHE}/${xcrypt}" -C "${BUILD}/xcrypt-src" --strip-components=1
    (cd "${BUILD}/xcrypt-src" && ./configure --prefix="${BUILD}/xcrypt" --libdir="${BUILD}/xcrypt/lib" \
        --enable-obsolete-api=glibc --disable-static > /dev/null && make -j"$(nproc)" > /dev/null \
        && make install > /dev/null)
    rm -rf "${BUILD}/xcrypt-src"
    libcrypt="${BUILD}/xcrypt/lib/libcrypt.so.1"
    version="$(objdump -T "${libcrypt}" | awk '$NF == "crypt" && $(NF - 1) ~ /GLIBC_/ {gsub(/[()]/, "", $(NF - 1)); print $(NF - 1)}')"
    [ -n "${version}" ]
    printf 'char *crypt_versioned(const char *, const char *);\n__asm__(".symver crypt_versioned,crypt@%s");\nchar *crypt(const char *key, const char *salt) { return crypt_versioned(key, salt); }\n' \
        "${version}" > "${BUILD}/crypt.c"
    cc -O2 -fPIC -c "${BUILD}/crypt.c" -o "${BUILD}/crypt.o"
    rm -f "${BUILD}/libcrypt-versioned.a"
    ar rcs "${BUILD}/libcrypt-versioned.a" "${BUILD}/crypt.o"
    CRYPT_LIBS="${BUILD}/libcrypt-versioned.a ${libcrypt}"
    CRYPT_CPPFLAGS=" -I${BUILD}/xcrypt/include"
fi
(cd "${BUILD}/src" && CFLAGS="-O2 -fPIC" CXXFLAGS="-O2 -fPIC" CPPFLAGS="-I${DEPS}/include${CRYPT_CPPFLAGS}" LDFLAGS="-L${DEPS}/lib" LIBS="${CRYPT_LIBS}" ./configure \
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
if [ -e "${BUILD}/xcrypt/lib/libcrypt.so.1" ]; then
    mkdir -p "${TREE}/lib/selkies"
    cp -L "${BUILD}/xcrypt/lib/libcrypt.so.1" "${TREE}/lib/selkies/libcrypt.so.1"
fi
readelf -d "${OUT}/sbin/cupsd" | grep NEEDED
