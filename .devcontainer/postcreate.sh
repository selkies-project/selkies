#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Faithful repo development setup: builds the web client from source and then
# installs the Python package editable with the web client bundled, exactly as
# the CI wheel build does.
set -e

cd "${WORKSPACE_FOLDER:-/workspaces/selkies}"

# The same script the wheel build, the conda recipe, and the root Dockerfile run,
# so the bundle under src/selkies/selkies_web is the one every channel ships
./scripts/ci/build-web.sh

# The pinned pixelflux and pcmflux are a coordinated pre-release, published per
# commit to the GitHub releases, so the newest ones are fetched before the
# editable install resolves the pin; a pin they leave unmet is pip's to report.
WHEELS="$(mktemp -d)"
.devcontainer/features/desktop-selkies/src/fetch-release-wheels.sh "${WHEELS}" none

# The test extras ride along, so the checkout can validate itself
PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --retries 5 --timeout 60 \
  --user --find-links "${WHEELS}" -e ".[test]"
rm -rf "${WHEELS}"

# The rest of what the suites need: the Playwright engines with their system
# libraries, the OpenH264 plugin Firefox negotiates H.264 with, and the C
# helpers under tests/tools. Best effort, so a container without the network
# or the packages for them still has the editable install.
if python3 -m playwright install --with-deps chromium firefox webkit &&
    sh tests/tools/fetch-openh264.sh &&
    make -C tests/tools; then
  echo "The suites' browsers and tools are installed; scripts/ci/test-stack.sh starts their display."
else
  echo "warning: the suites' browsers or tools did not install; the integration and e2e tiers need them" >&2
fi
echo "Selkies installed editable with the bundled web client. Start it with: start-selkies.sh"
