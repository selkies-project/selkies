#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# ESLint over the dashboard, the web core, the touch gamepad and the scripts
# outside them, plus the dashboard's TypeScript check. Run from the
# repository root; requires npm.
#
# The CI lint gate and the pre-commit hook both call this, so the two cannot
# drift apart.
set -eu

test -f pyproject.toml

# The dashboard carries its own ESLint and eslint.config.js.
if [ ! -d addons/selkies-dashboard/node_modules ]; then
    # Retried like build-web.sh's install: npm gives a failed registry
    # fetch two attempts and no more.
    (cd addons/selkies-dashboard && npm ci --no-audit --no-fund \
        --fetch-retries=5 --fetch-retry-maxtimeout=120000)
fi
echo "eslint: addons/selkies-dashboard"
(cd addons/selkies-dashboard && npm run --silent lint)

# The web core and the touch gamepad have no dev dependencies of their own:
# addons/eslint.config.mjs lints them with the dashboard's ESLint.
echo "eslint: addons/selkies-web-core addons/universal-touch-gamepad"
(cd addons && selkies-dashboard/node_modules/.bin/eslint --max-warnings 0 \
    selkies-web-core universal-touch-gamepad)

# The Node audits, the site's scripts and the image tier's tester pages:
# eslint.config.mjs at the root, with the same ESLint.
echo "eslint: tests/tools website/scripts tests/image/site"
addons/selkies-dashboard/node_modules/.bin/eslint --max-warnings 0 \
    'tests/tools/**/*.mjs' 'website/scripts/**/*.mjs' 'tests/image/site/**/*.js'

# The dashboard is the typed package. Its own build runs tsc as well, but
# that is downstream of this gate.
echo "tsc: addons/selkies-dashboard"
(cd addons/selkies-dashboard && npm run --silent typecheck)
