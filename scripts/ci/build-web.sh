#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Bundle the web client into src/selkies/selkies_web, the directory the wheel
# ships as package data. Run from the repository root; requires npm.
#
# Both the wheel build and the conda recipe call this, so the browser payload
# is identical in every distribution channel.
set -eux

test -f pyproject.toml

npm_install() {
    # Lockfiles are gitignored in this repository, so `npm ci` has nothing to
    # install from
    npm install --no-audit --no-fund
}

# The core is built first: both dashboards take it, and the gamepad DB built
# alongside it, out of its dist through their own copy-core.js/copy-jsdb.js
# build steps.
(cd addons/selkies-web-core && npm_install && npm run build)
(cd addons/selkies-dashboard && npm_install && SELKIES_INJECT=1 npm run build)

# The Wish dashboard is an alternative front end: it is not what the wheel
# ships, but it is built here so a break in it fails the build, and so the
# dashboard e2e tier has a bundle to serve
(cd addons/selkies-dashboard-wish && npm_install && npm run build)

mkdir -p addons/selkies-dashboard/dist/src
cp addons/selkies-web-core/dist/selkies-core.js addons/selkies-dashboard/dist/src/
cp addons/universal-touch-gamepad/universalTouchGamepad.js addons/selkies-dashboard/dist/src/

# .gitignore keeps src/selkies/selkies_web out of git; it is generated here
rm -rf src/selkies/selkies_web
cp -ar addons/selkies-dashboard/dist src/selkies/selkies_web

# start_url is relative so an installed client launches back into the subfolder
# it was served from, which an absolute "/" would discard.
printf '%s' '{"name":"Selkies","short_name":"Selkies","display":"fullscreen","background_color":"#000000","theme_color":"#000000","icons":[{"src":"icon-512.png","type":"image/png","sizes":"512x512"}],"start_url":"."}' > src/selkies/selkies_web/manifest.json
# PWA icon/favicon are vendored in this repository, not downloaded. The plated
# icon belongs to the installed app alone; icon.png stays the bare mark the
# dashboard ships, because that is what the browser tab draws.
cp docs/assets/logo/icon-512x512.png src/selkies/selkies_web/icon-512.png
cp docs/assets/logo/favicon.ico src/selkies/selkies_web/favicon.ico

test -f src/selkies/selkies_web/index.html
