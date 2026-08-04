#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>-r0.apk (run inside an Alpine container)
set -eux
apk add --no-cache abuild python3 py3-pip py3-virtualenv ca-certificates
rm -rf /pkg-root && mkdir -p /pkg-root/opt /pkg-root/usr/bin
/repo/infra/packaging/mkvenv.sh
sed -i "s/^pkgver=.*/pkgver=${SELKIES_VERSION:-0.0.0}/" /repo/infra/packaging/apk/APKBUILD
abuild-keygen -a -i
export REPODEST=/out
mkdir -p /out
cd /repo/infra/packaging/apk && abuild -F package
