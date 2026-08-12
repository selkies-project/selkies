#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>-1-<arch>.pkg.tar.zst (run inside an Arch container)
set -eux
# base-devel: dependencies without a wheel for this distribution's Python are
# compiled from their sdist. libxkbcommon is loaded with ctypes at runtime and
# lets mkvenv.sh's smoke test exercise that path
pacman -Syu --noconfirm --needed python python-pip base-devel libxkbcommon sudo
/repo/infra/packaging/mkvenv.sh
/repo/infra/packaging/interposer.sh /pkg-root
# makepkg writes src/ and pkg/ next to the PKGBUILD, and /repo is read-only
rm -rf /build
mkdir -p /build /out
cp -r /repo/infra/packaging/arch /build/arch
chmod -R u+w /build/arch
# makepkg forbids hyphens in pkgver. pacman sorts a PEP 440 pre-release suffix
# below the bare release on its own, so nothing else here needs translating.
PKGVER="$(printf '%s' "${SELKIES_VERSION:-0.0.0}" | tr '-' '.')"
sed -i "s/^pkgver=.*/pkgver=${PKGVER}/" /build/arch/PKGBUILD
# makepkg refuses to run as root
useradd -m builder 2>/dev/null || true
chown -R builder:builder /build /out
su builder -c "cd /build/arch && PKGDEST=/out makepkg -f --nodeps"
ls -la /out
