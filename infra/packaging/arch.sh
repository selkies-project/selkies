#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build selkies-<ver>-1-<arch>.pkg.tar.zst (run inside an Arch container)
set -eux
pacman -Syu --noconfirm --needed python python-pip base-devel sudo
rm -rf /pkg-root && mkdir -p /pkg-root/opt /pkg-root/usr/bin
/repo/infra/packaging/mkvenv.sh
sed -i "s/^pkgver=.*/pkgver=${SELKIES_VERSION:-0.0.0}/" /repo/infra/packaging/arch/PKGBUILD
useradd -m builder || true
chown -R builder:builder /repo
mkdir -p /out
su builder -c "cd /repo/infra/packaging/arch && PKGDEST=/out makepkg -f"
