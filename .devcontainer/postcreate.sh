#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Faithful repo development setup: builds the web client and the server from
# this checkout, as CI does, and installs what the suites need beside them, so
# the checkout can validate itself.
set -e

cd "${WORKSPACE_FOLDER:-/workspaces/selkies}"

# The same script every channel builds the bundle with, so the bundle in
# dist/web, which a server built from this checkout serves, is the one every
# channel ships
./scripts/ci/build-web.sh

# The toolchain and the libraries the server links, then the server the suites
# start (target/release/selkies)
./scripts/ci/server-build-deps.sh
PATH="${CARGO_HOME:-${HOME}/.cargo}/bin:${PATH}"
cargo build --release --locked -p selkies

# The suites' own dependencies, and the pixelflux and pcmflux they drive beside
# the server: local wheels of the same sources the server links.
WHEELS="$(mktemp -d)"
python3 -m pip wheel --no-deps --wheel-dir "${WHEELS}" ./crates/pixelflux ./crates/pcmflux
PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --retries 5 --timeout 60 --user -r tests/requirements.txt
PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --user --no-deps --force-reinstall "${WHEELS}"/*.whl
rm -rf "${WHEELS}"

# The rest of what the suites need: the Playwright engines with their system
# libraries, the OpenH264 plugin Firefox negotiates H.264 with, and the C
# helpers under tests/tools. Best effort, so a container without the network
# or the packages for them still has the server.
if python3 -m playwright install --with-deps chromium firefox webkit &&
    sh tests/tools/fetch-openh264.sh &&
    make -C tests/tools; then
  echo "The suites' browsers and tools are installed; scripts/ci/test-stack.sh starts their display."
else
  echo "warning: the suites' browsers or tools did not install; the integration and e2e tiers need them" >&2
fi
echo "Selkies is built from the checkout at target/release/selkies, and serves the web client in dist/web."
