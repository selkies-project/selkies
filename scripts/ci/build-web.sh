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
    # `npm ci` has nothing to install from: lockfiles are gitignored here. The
    # retry flags belong on the command because two of this script's four callers
    # run on a bare runner with no image to configure, and npm's own two retries
    # do not carry a dependency tree this size fetched three times over.
    npm install --no-audit --no-fund --fetch-retries=5 --fetch-retry-maxtimeout=120000
}

# The core is built first: both dashboards take it, and the gamepad DB built
# alongside it, out of its dist through their own copy-core.js/copy-jsdb.js
# build steps.
(cd addons/selkies-web-core && npm_install && npm run build)

# The Wish dashboard is an alternative front end the wheel does not ship, built
# here so a break in it fails the build and the dashboard e2e tier has a bundle.
# Installs run in turn so two npm processes never write one cache entry; the
# builds share only the core's dist, already there, so they run together.
(cd addons/selkies-dashboard && npm_install)
(cd addons/selkies-dashboard-wish && npm_install)
(cd addons/selkies-dashboard && SELKIES_INJECT=1 npm run build) &
classic_build=$!
(cd addons/selkies-dashboard-wish && npm run build) &
wish_build=$!
# Each is waited on by pid, so `set -e` sees the failing one rather than the
# exit status of whichever finished last.
wait "${classic_build}"
wait "${wish_build}"

mkdir -p addons/selkies-dashboard/dist/src
cp addons/selkies-web-core/dist/selkies-core.js addons/selkies-dashboard/dist/src/
cp addons/universal-touch-gamepad/universalTouchGamepad.js addons/selkies-dashboard/dist/src/

# .gitignore keeps src/selkies/selkies_web out of git; it is generated here
rm -rf src/selkies/selkies_web
cp -ar addons/selkies-dashboard/dist src/selkies/selkies_web

# A regular package rather than an implicit namespace one: importlib.resources
# on Python 3.9 cannot locate the files of a namespace package, and that is how
# the server reads the bundled client.
printf '%s\n' '"""Bundled web client, served by the stream server as package data."""' > src/selkies/selkies_web/__init__.py

# start_url is relative so an installed client launches back into the subfolder
# it was served from, which an absolute "/" would discard.
printf '%s' '{"name":"Selkies","short_name":"Selkies","display":"fullscreen","background_color":"#000000","theme_color":"#000000","icons":[{"src":"icon-512.png","type":"image/png","sizes":"512x512"}],"start_url":"."}' > src/selkies/selkies_web/manifest.json
# PWA icon/favicon are vendored in this repository, not downloaded. The plated
# icon belongs to the installed app alone; icon.png stays the bare mark the
# dashboard ships, because that is what the browser tab draws.
cp docs/assets/logo/icon-512x512.png src/selkies/selkies_web/icon-512.png
cp docs/assets/logo/favicon.ico src/selkies/selkies_web/favicon.ico

test -f src/selkies/selkies_web/index.html
