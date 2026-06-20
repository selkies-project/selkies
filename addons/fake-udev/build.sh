#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

set -e

# Create output directory
PKG_DIR="/opt/${PKG_NAME?missing env}_${PKG_VERSION?missing env}"
mkdir -p "${PKG_DIR}/DEBIAN"
# Handles normalising i.*86 values for native arch, if any
LIB_DIR="${PKG_DIR}/usr/lib/$(gcc -print-multiarch | sed -e 's/i.*86/i386/')"
mkdir -p "${LIB_DIR}"
export LIB_VERSION="${PKG_VERSION}-fake"
make
cp -f libudev.so.${PKG_VERSION}-fake "${LIB_DIR}/libudev.so.${PKG_VERSION}-fake"

if [ "$(dpkg --print-architecture)" = "amd64" ]; then
    LIB_DIR="${PKG_DIR}/usr/lib/$(gcc -m32 -print-multiarch | sed -e 's/i.*86/i386/')"
    mkdir -p "${LIB_DIR}"
    make all32
    cp -f libudev_x86.so.${PKG_VERSION}-fake "${LIB_DIR}/libudev.so.${PKG_VERSION}-fake"
fi

PKG_SIZE="$(du -s "${PKG_DIR}/usr" | awk '{print $1}' | xargs)"

cat - > ${PKG_DIR}/DEBIAN/control <<EOF
Package: ${PKG_NAME?missing env}
Version: ${PKG_VERSION}
Section: custom
Priority: optional
Architecture: $(dpkg --print-architecture)
Essential: no
Installed-Size: ${PKG_SIZE?missing env}
Maintainer: ${DEBFULLNAME?missing env} <${DEBEMAIL?missing env}>
Description: Fake udev shared library for Selkies project. A dependency for Selkies joystick device interposer.
EOF

dpkg-deb --build ${PKG_DIR}
