#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>-r0.apk (run inside an Alpine container)
set -eux
# build-base/python3-dev/linux-headers: musl has no manylinux wheels, so every
# extension dependency is compiled here (psutil needs the kernel headers).
apk add --no-cache \
    abuild build-base pkgconf linux-headers \
    python3 python3-dev py3-pip py3-virtualenv \
    ca-certificates
# The runtime libraries come from the APKBUILD's own depends, keeping one list:
# the venv links them, mkvenv.sh's smoke test loads them, and abuild resolves the
# sonames it scans against whatever the builder has installed.
# shellcheck disable=SC2046  # the depends list is deliberately word-split
apk add --no-cache $(sed -n 's/^depends="\(.*\)"$/\1/p' /repo/infra/packaging/apk/APKBUILD)
. /repo/infra/packaging/version.sh
/repo/infra/packaging/mkvenv.sh
/repo/infra/packaging/interposer.sh /pkg-root
# abuild writes src/ and pkg/ next to the APKBUILD, and /repo is read-only
rm -rf /build
mkdir -p /build /out
cp -r /repo/infra/packaging/apk /build/apk
chmod -R u+w /build/apk
# Alpine spells pre-releases such as the 0.0.0.dev0 CI default its own way
PKGVER="$(alpine_version "${SELKIES_VERSION:-0.0.0}")"
sed -i "s/^pkgver=.*/pkgver=${PKGVER}/" /build/apk/APKBUILD
# -i would install the public key with doas, which this image has no need for:
# the build runs as root and can place the key itself
abuild-keygen -a -n
cp -f "${HOME}"/.abuild/*.rsa.pub /etc/apk/keys/
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
