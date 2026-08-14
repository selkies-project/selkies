#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Faithful repo development setup: builds the web client from source and then
# installs the Python package editable with the web client bundled, exactly as
# the CI wheel build does.
set -e

cd "${WORKSPACE_FOLDER:-/workspaces/selkies}"

cd addons/selkies-web-core && npm install --no-audit --no-fund && npm run build && cd ../..
cd addons/selkies-dashboard && cp ../selkies-web-core/dist/selkies-core.js src/ && npm install --no-audit --no-fund && SELKIES_INJECT=1 npm run build && cd ../..
mkdir -p addons/selkies-dashboard/dist/src
cp addons/selkies-web-core/dist/selkies-core.js addons/selkies-dashboard/dist/src/
cp addons/universal-touch-gamepad/universalTouchGamepad.js addons/selkies-dashboard/dist/src/
cp -r addons/selkies-web-core/dist/jsdb addons/selkies-dashboard/dist/
cp -ar addons/selkies-dashboard/dist src/selkies/selkies_web
printf '%s' '{"name":"Selkies","short_name":"Selkies","display":"fullscreen","background_color":"#000000","theme_color":"#000000","icons":[{"src":"icon-512.png","type":"image/png","sizes":"512x512"}],"start_url":"."}' > src/selkies/selkies_web/manifest.json
cp docs/assets/logo/icon-512x512.png src/selkies/selkies_web/icon-512.png
cp docs/assets/logo/favicon.ico src/selkies/selkies_web/favicon.ico

PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --user -e .
echo "Selkies installed editable with the bundled web client. Start it with: start-selkies.sh"
