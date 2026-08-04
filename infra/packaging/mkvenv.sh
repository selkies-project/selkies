#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
# Create the self-contained Selkies prefix at /pkg-root/opt/selkies from the
# wheel in /dist, targeting the distro's system Python.
set -eux
python3 -m venv /pkg-root/opt/selkies
/pkg-root/opt/selkies/bin/pip install --no-cache-dir --upgrade pip
/pkg-root/opt/selkies/bin/pip install --no-cache-dir /dist/selkies-*-py3-none-any.whl
mkdir -p /pkg-root/usr/bin
ln -s /opt/selkies/bin/selkies /pkg-root/usr/bin/selkies
ln -s /opt/selkies/bin/selkies-resize /pkg-root/usr/bin/selkies-resize
