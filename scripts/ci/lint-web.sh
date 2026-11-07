#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# ESLint over the dashboard, the web core, the touch gamepad and the scripts
# outside them, plus the TypeScript check of the dashboard and of the web
# core. Run from the
# repository root; requires npm.
#
# The CI lint gate and the pre-commit hook both call this, so the two cannot
# drift apart.
set -eu

test -f Cargo.toml

# The dashboard carries its own ESLint and eslint.config.js.
pkg=addons/selkies-dashboard
if [ ! -d "${pkg}/node_modules" ]; then
    # Retried like build-web.sh's install: npm gives a failed registry
    # fetch two attempts and no more.
    (cd "${pkg}" && npm ci --no-audit --no-fund \
        --fetch-retries=5 --fetch-retry-maxtimeout=120000)
fi
echo "eslint: ${pkg}"
(cd "${pkg}" && npm run --silent lint)

# The web core and the touch gamepad are linted by the dashboard's ESLint, whose
# addons/eslint.config.mjs covers both; the web core's TypeScript is checked below.
echo "eslint: addons/selkies-web-core addons/universal-touch-gamepad"
(cd addons && selkies-dashboard/node_modules/.bin/eslint --max-warnings 0 \
    selkies-web-core universal-touch-gamepad)

# The Node audits, the site's scripts and the image tier's tester pages:
# eslint.config.mjs at the root, with the same ESLint.
echo "eslint: tests/tools website/scripts tests/image/site"
addons/selkies-dashboard/node_modules/.bin/eslint --max-warnings 0 \
    'tests/tools/**/*.mjs' 'website/scripts/**/*.mjs' 'tests/image/site/**/*.js'

# The dashboard's own build runs tsc as well, but that is downstream of this gate.
echo "tsc: addons/selkies-dashboard"
(cd addons/selkies-dashboard && npm run --silent typecheck)

# The web core checks itself strictly with the TypeScript it declares; its own
# node_modules, unlike the lint's, exist only once its dependencies are installed.
echo "tsc: addons/selkies-web-core"
core=addons/selkies-web-core
if [ ! -d "${core}/node_modules" ]; then
    (cd "${core}" && npm ci --no-audit --no-fund \
        --fetch-retries=5 --fetch-retry-maxtimeout=120000)
fi
(cd "${core}" && npm run --silent typecheck)
