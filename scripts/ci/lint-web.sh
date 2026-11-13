#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# ESLint over the dashboard, the web core, the touch gamepad and the scripts
# outside them, plus the TypeScript check of the dashboard, the web core, the
# Node audits, the BrowserStack smoke and the site. Run from the repository
# root; requires npm.
#
# The CI lint gate and the pre-commit hook both call this, so the two cannot
# drift apart.
set -eu

test -f Cargo.toml

# The configs are TypeScript, which ESLint loads through Node's own type
# stripping under this flag.
flag=unstable_native_nodejs_ts_config

# The dashboard carries its own ESLint and eslint.config.ts.
pkg=web/selkies-dashboard
if [ ! -d "${pkg}/node_modules" ]; then
    # Retried like build-web.sh's install: npm gives a failed registry
    # fetch two attempts and no more.
    (cd "${pkg}" && npm ci --no-audit --no-fund \
        --fetch-retries=5 --fetch-retry-maxtimeout=120000)
fi

# The web core's build helpers are linted and checked against the Vite it
# declares, so its dependencies are installed before either runs.
core=web/selkies-web-core
if [ ! -x "${core}/node_modules/.bin/tsc" ]; then
    (cd "${core}" && npm ci --no-audit --no-fund \
        --fetch-retries=5 --fetch-retry-maxtimeout=120000)
fi

echo "eslint: ${pkg}"
(cd "${pkg}" && npm run --silent lint)

# The web core and the touch gamepad are linted by the dashboard's ESLint, whose
# web/eslint.config.mts covers both; the web core's TypeScript is checked below.
echo "eslint: web/selkies-web-core web/universal-touch-gamepad"
(cd web && selkies-dashboard/node_modules/.bin/eslint --flag "${flag}" --max-warnings 0 \
    selkies-web-core universal-touch-gamepad eslint.config.mts)

# The site's scripts are linted against their types, which its dependencies
# carry.
site=website
if [ ! -x "${site}/node_modules/.bin/tsc" ]; then
    (cd "${site}" && npm ci --no-audit --no-fund \
        --fetch-retries=5 --fetch-retry-maxtimeout=120000)
fi

# The Node audits, the BrowserStack smoke, the site's scripts and configs, the
# image tier's tester pages and this config itself: eslint.config.mts at the
# root, with the same ESLint.
echo "eslint: tests/tools tests/browserstack website tests/image/site eslint.config.mts"
web/selkies-dashboard/node_modules/.bin/eslint --flag "${flag}" --max-warnings 0 \
    'tests/tools/**/*.mjs' 'tests/tools/**/*.ts' 'tests/browserstack/*.ts' 'website/scripts/**/*.ts' \
    website/next.config.ts website/postcss.config.ts 'tests/image/site/**/*.js' eslint.config.mts

# The dashboard's own build runs tsc as well, but that is downstream of this gate.
echo "tsc: web/selkies-dashboard"
(cd web/selkies-dashboard && npm run --silent typecheck)

# The web core checks itself strictly with the TypeScript it declares.
echo "tsc: web/selkies-web-core"
(cd "${core}" && npm run --silent typecheck)

# The Node audits check themselves and the web-core modules they import
# against Node's types beside the DOM's, with the dashboard's TypeScript.
echo "tsc: tests/tools"
"${pkg}/node_modules/.bin/tsc" -p tests/tools

# The BrowserStack smoke checks itself with the TypeScript it declares, against
# the WebdriverIO and Node types its own dependencies carry.
echo "tsc: tests/browserstack"
smoke=tests/browserstack
if [ ! -x "${smoke}/node_modules/.bin/tsc" ]; then
    (cd "${smoke}" && npm ci --no-audit --no-fund \
        --fetch-retries=5 --fetch-retry-maxtimeout=120000)
fi
(cd "${smoke}" && npm run --silent typecheck)

# The site checks its pages, its build scripts and its configs with the
# TypeScript it declares, after generating the route types they name.
echo "tsc: website"
(cd "${site}" && npm run --silent typecheck)
