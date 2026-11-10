#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# ESLint over the dashboard, the web core, the touch gamepad and the scripts
# outside them, plus the TypeScript check of the dashboard, the web core and
# the BrowserStack smoke. Run from the repository root; requires npm.
#
# The CI lint gate and the pre-commit hook both call this, so the two cannot
# drift apart.
set -eu

test -f Cargo.toml

# The dashboard carries its own ESLint and eslint.config.js.
pkg=web/selkies-dashboard
if [ ! -d "${pkg}/node_modules" ]; then
    # Retried like build-web.sh's install: npm gives a failed registry
    # fetch two attempts and no more.
    (cd "${pkg}" && npm ci --no-audit --no-fund \
        --fetch-retries=5 --fetch-retry-maxtimeout=120000)
fi
echo "eslint: ${pkg}"
(cd "${pkg}" && npm run --silent lint)

# The web core and the touch gamepad are linted by the dashboard's ESLint, whose
# web/eslint.config.mjs covers both; the web core's TypeScript is checked below.
echo "eslint: web/selkies-web-core web/universal-touch-gamepad"
(cd web && selkies-dashboard/node_modules/.bin/eslint --max-warnings 0 \
    selkies-web-core universal-touch-gamepad)

# The Node audits, the BrowserStack smoke, the site's scripts and the image
# tier's tester pages: eslint.config.mjs at the root, with the same ESLint.
echo "eslint: tests/tools tests/browserstack website/scripts tests/image/site"
web/selkies-dashboard/node_modules/.bin/eslint --max-warnings 0 \
    'tests/tools/**/*.mjs' 'tests/browserstack/*.ts' 'website/scripts/**/*.mjs' 'tests/image/site/**/*.js'

# The dashboard's own build runs tsc as well, but that is downstream of this gate.
echo "tsc: web/selkies-dashboard"
(cd web/selkies-dashboard && npm run --silent typecheck)

# The web core checks itself strictly with the TypeScript it declares; its own
# node_modules, unlike the lint's, exist only once its dependencies are installed.
echo "tsc: web/selkies-web-core"
core=web/selkies-web-core
if [ ! -x "${core}/node_modules/.bin/tsc" ]; then
    (cd "${core}" && npm ci --no-audit --no-fund \
        --fetch-retries=5 --fetch-retry-maxtimeout=120000)
fi
(cd "${core}" && npm run --silent typecheck)

# The BrowserStack smoke checks itself with the TypeScript it declares, against
# the WebdriverIO and Node types its own dependencies carry.
echo "tsc: tests/browserstack"
smoke=tests/browserstack
if [ ! -x "${smoke}/node_modules/.bin/tsc" ]; then
    (cd "${smoke}" && npm ci --no-audit --no-fund \
        --fetch-retries=5 --fetch-retry-maxtimeout=120000)
fi
(cd "${smoke}" && npm run --silent typecheck)
