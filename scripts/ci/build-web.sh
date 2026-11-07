#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Bundle the web client into dist/web, the directory the server reads it from
# in a checkout and the images and the AppImage install at share/selkies/web
# beside the binary. Run from the repository root; requires npm.
#
# Every image and package builds the bundle through this script, so the browser
# payload is identical in every distribution channel.
set -eux

test -f Cargo.toml

npm_install() {
    # From the committed lockfile, so every channel installs one resolved tree.
    # The retry flags belong on the command because the callers that run on a
    # bare runner have no image to configure, and npm's own two retries do not
    # carry a dependency tree this size fetched three times over.
    npm ci --no-audit --no-fund --fetch-retries=5 --fetch-retry-maxtimeout=120000
}

# The core is built first: the dashboard takes it, and the gamepad DB and WASM
# codecs built alongside it, out of its dist through its own
# copy-core.js/copy-assets.js build steps.
(cd addons/selkies-web-core && npm_install && npm run build)

(cd addons/selkies-dashboard && npm_install && SELKIES_INJECT=1 npm run build)

# .gitignore keeps dist out of git; it is generated here
rm -rf dist/web
mkdir -p dist
cp -ar addons/selkies-dashboard/dist dist/web

# start_url is relative so an installed client launches back into the subfolder
# it was served from, which an absolute "/" would discard.
printf '%s' '{"name":"Selkies","short_name":"Selkies","display":"fullscreen","background_color":"#000000","theme_color":"#000000","icons":[{"src":"icon-512.png","type":"image/png","sizes":"512x512"}],"start_url":"."}' > dist/web/manifest.json
# PWA icon/favicon are vendored in this repository, not downloaded. The plated
# icon belongs to the installed app alone; icon.png stays the bare mark the
# dashboard ships, because that is what the browser tab draws.
cp docs/assets/logo/icon-512x512.png dist/web/icon-512.png
cp docs/assets/logo/favicon.ico dist/web/favicon.ico

test -f dist/web/index.html
