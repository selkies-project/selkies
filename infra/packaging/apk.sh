#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>-r0.apk (run inside an Alpine container)
set -eux

# Package managers with no retry option of their own -- apk, dnf, pacman, and
# RubyGems all lack one -- are bounded-retried here. This composes with whatever
# internal retrying the tool already does rather than replacing it, so it cannot
# lower a default the way an explicit --setopt could.
retry() {
    i=1
    until "$@"; do
        [ "${i}" -ge 5 ] && return 1
        i=$((i + 1)); sleep 5
    done
}
# The server's build against Alpine's own musl builds of every library it
# links. pipewire-dev gives the V4L2 interposer its PipeWire frame source
# (headers only; the library is loaded at runtime when an application uses it)
retry apk add --no-cache \
    abuild build-base pkgconf linux-headers pipewire-dev ca-certificates curl git \
    cmake nasm clang \
    x265-dev svt-av1-dev dav1d-dev libde265-dev \
    libva-dev libdrm-dev mesa-dev wayland-dev libinput-dev eudev-dev libxkbcommon-dev \
    pixman-dev libxcb-dev pulseaudio-dev
# bindgen opens libclang at run time, and Alpine names that package after the
# compiler's major version, which moves every release
retry apk add --no-cache "clang$(clang -dumpversion | cut -d. -f1)-libclang"
# shellcheck source=infra/packaging/version.sh
. /repo/infra/packaging/version.sh
rm -rf /pkg-root
/repo/infra/packaging/server.sh /pkg-root
/repo/infra/packaging/interposer.sh /pkg-root
/repo/infra/packaging/v4l2-interposer.sh /pkg-root
# abuild writes src/ and pkg/ next to the APKBUILD, and /repo is read-only
rm -rf /build/apk /build/apkrepo
mkdir -p /build /out
cp -r /repo/infra/packaging/apk /build/apk
chmod -R u+w /build/apk
# Alpine spells pre-releases such as the 0.0.0.dev0 CI default its own way
PKGVER="$(alpine_version "${SELKIES_VERSION:-0.0.0}")"
sed -i "s/^pkgver=.*/pkgver=${PKGVER}/" /build/apk/APKBUILD
# abuild adds a so: dependency for every library the binary links on its own;
# the packages behind the libraries loaded by name at run time are named here
DEPENDS="$(/repo/infra/packaging/runtime-deps.sh /pkg-root/usr/bin/selkies | tr '\n' ' ')"
sed -i "s/^depends=.*/depends=\"${DEPENDS% }\"/" /build/apk/APKBUILD
# -i would install the public key with doas, which this image has no need for:
# the build runs as root and can place the key itself
abuild-keygen -a -n
cp -f "${HOME}"/.abuild/*.rsa.pub /etc/apk/keys/
# rootpkg is the target that assembles the .apk; `package` alone only stages
# $pkgdir
REPODEST="/build/apkrepo"
export REPODEST
cd /build/apk && abuild -F rootpkg
# abuild's filename has no architecture in it, and both arch jobs land their
# .apk in the same release directory; every package file is
# selkies-<version>-<architecture>.<format>, the version as the release tag spells
# it, while apk reads its own (2.0.0_rc0-r0) from inside the archive
# shellcheck disable=SC2046  # abuild filenames never contain spaces
set -- $(find /build/apkrepo -name 'selkies-*.apk')
[ "$#" -eq 1 ] || { echo "abuild produced $# .apk files, expected one" >&2; exit 1; }
cp "$1" "/out/selkies-${SELKIES_VERSION:-0.0.0}-$(uname -m).apk"
ls -la /out
