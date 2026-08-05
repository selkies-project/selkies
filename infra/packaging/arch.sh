#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>-1-<arch>.pkg.tar.zst (run inside an Arch container)
set -eux
# libxkbcommon: the xkbcommon dependency publishes an sdist only and compiles a
# cffi extension against the system headers (base-devel supplies the toolchain)
pacman -Syu --noconfirm --needed python python-pip base-devel libxkbcommon sudo
/repo/infra/packaging/mkvenv.sh
# makepkg writes src/ and pkg/ next to the PKGBUILD, and /repo is read-only
rm -rf /build
mkdir -p /build /out
cp -r /repo/infra/packaging/arch /build/arch
chmod -R u+w /build/arch
# Arch pkgver forbids hyphens
PKGVER="$(printf '%s' "${SELKIES_VERSION:-0.0.0}" | tr '-' '.')"
sed -i "s/^pkgver=.*/pkgver=${PKGVER}/" /build/arch/PKGBUILD
# makepkg refuses to run as root
useradd -m builder 2>/dev/null || true
chown -R builder:builder /build /out
su builder -c "cd /build/arch && PKGDEST=/out makepkg -f --nodeps"
ls -la /out
