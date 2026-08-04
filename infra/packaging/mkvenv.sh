#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Create the self-contained Selkies prefix at /pkg-root/opt/selkies from the
# wheel in /dist, targeting the distro's system Python.
set -eux
python3 -m venv /pkg-root/opt/selkies
/pkg-root/opt/selkies/bin/pip install --no-cache-dir --upgrade pip
WHEELS="$(ls /dist/selkies-*-py3-none-any.whl)"
# Fresh pixelflux/pcmflux wheels (built from the master HEAD of
# linuxserver/*) ride along in /dist; pip selects the one matching this
# distro's Python and platform, and anything absent resolves from PyPI
if ls /dist/pixelflux-*.whl > /dev/null 2>&1; then
  /pkg-root/opt/selkies/bin/pip download --no-cache-dir --no-deps --no-index --find-links /dist --dest /tmp/picked pixelflux pcmflux
  WHEELS="${WHEELS} $(ls /tmp/picked/*.whl)"
fi
# Wheel file names never contain spaces; word-splitting is intentional here
# shellcheck disable=SC2086
/pkg-root/opt/selkies/bin/pip install --no-cache-dir ${WHEELS}
mkdir -p /pkg-root/usr/bin
ln -s /opt/selkies/bin/selkies /pkg-root/usr/bin/selkies
ln -s /opt/selkies/bin/selkies-resize /pkg-root/usr/bin/selkies-resize
