#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Faithful repo development setup: builds the web client from source and then
# installs the Python package editable with the web client bundled, exactly as
# the CI wheel build does.
set -e

cd "${WORKSPACE_FOLDER:-/workspaces/selkies}"

# The same script the wheel build, the conda recipe and the root Dockerfile run,
# so the bundle under src/selkies/selkies_web is the one every channel ships
./scripts/ci/build-web.sh

PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --user -e .
echo "Selkies installed editable with the bundled web client. Start it with: start-selkies.sh"
