#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Build the self-contained Selkies prefix from the wheel in /dist and stage it
# under /pkg-root for the packaging tools. The venv is created at its final
# install path (/opt/selkies) and only moved afterwards, so console-script
# shebangs and pyvenv.cfg point where the package actually lands.
set -eux
rm -rf /opt/selkies /pkg-root
python3 -m venv /opt/selkies
/opt/selkies/bin/pip install --no-cache-dir --upgrade pip
WHEELS="$(ls /dist/selkies-*-py3-none-any.whl)"
# CI drops pixelflux/pcmflux wheels built from the master HEAD of linuxserver/*
# into /dist; pick the one matching this distro's Python and platform. Either
# one that is absent resolves from PyPI as a dependency of the selkies wheel.
mkdir -p /tmp/picked
for pkg in pixelflux pcmflux; do
  if ls "/dist/${pkg}"-*.whl > /dev/null 2>&1; then
    /opt/selkies/bin/pip download --no-cache-dir --no-deps --no-index \
        --find-links /dist --dest /tmp/picked "${pkg}"
  fi
done
if ls /tmp/picked/*.whl > /dev/null 2>&1; then
  WHEELS="${WHEELS} $(ls /tmp/picked/*.whl)"
fi
# Wheel file names never contain spaces; word-splitting is intentional here
# shellcheck disable=SC2086
/opt/selkies/bin/pip install --no-cache-dir ${WHEELS}
# Fails the package build rather than shipping a prefix that cannot start
/opt/selkies/bin/selkies --help > /dev/null
mkdir -p /pkg-root/opt /pkg-root/usr/bin
mv /opt/selkies /pkg-root/opt/selkies
if grep -rIl /pkg-root /pkg-root/opt/selkies/bin; then
  echo "the staging path leaked into the packaged prefix" >&2
  exit 1
fi
# Every console script the wheel installs, so a native package puts the same
# commands on PATH as a pip install does. The links point at the install path,
# so they resolve once the package is unpacked rather than here.
for cmd in /pkg-root/opt/selkies/bin/selkies*; do
  name="$(basename "${cmd}")"
  ln -s "/opt/selkies/bin/${name}" "/pkg-root/usr/bin/${name}"
done
