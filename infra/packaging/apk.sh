#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>-r0.apk (run inside an Alpine container)
set -eux
# build-base/python3-dev/libxkbcommon-dev: the xkbcommon dependency publishes an
# sdist only and compiles a cffi extension against the system headers
apk add --no-cache \
    abuild build-base pkgconf \
    python3 python3-dev py3-pip py3-virtualenv \
    libxkbcommon-dev ca-certificates
/repo/infra/packaging/mkvenv.sh
# abuild writes src/ and pkg/ next to the APKBUILD, and /repo is read-only
rm -rf /build
mkdir -p /build /out
cp -r /repo/infra/packaging/apk /build/apk
chmod -R u+w /build/apk
# Alpine rejects PEP 440 suffixes such as the 0.0.0.dev0 CI default
PKGVER="$(printf '%s' "${SELKIES_VERSION:-0.0.0}" | sed -e 's/[^0-9.].*$//' -e 's/\.$//')"
sed -i "s/^pkgver=.*/pkgver=${PKGVER}/" /build/apk/APKBUILD
abuild-keygen -a -i -n
# rootpkg is the target that assembles the .apk; `package` alone only stages
# $pkgdir
REPODEST=/build/apkrepo
export REPODEST
cd /build/apk && abuild -F rootpkg
# abuild's filename has no architecture in it, and both arch jobs land their
# .apk in the same release directory
# shellcheck disable=SC2046  # abuild filenames never contain spaces
set -- $(find /build/apkrepo -name 'selkies-*.apk')
[ "$#" -gt 0 ] || { echo "abuild produced no .apk" >&2; exit 1; }
for apk in "$@"; do
  cp "${apk}" "/out/$(basename "${apk}" .apk)-$(uname -m).apk"
done
ls -la /out
