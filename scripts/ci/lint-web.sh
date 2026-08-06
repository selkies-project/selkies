#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# ESLint over the packages that carry a flat config, plus the Wish dashboard's
# TypeScript check. Run from the repository root; requires npm.
#
# The CI lint gate and the pre-commit hook both call this, so the two cannot
# drift apart.
set -eu

test -f pyproject.toml

# Only these two packages have an eslint.config.js. The web core and the
# touch-gamepad addon are linted by neither.
for pkg in addons/selkies-dashboard addons/selkies-dashboard-wish; do
    if [ ! -d "${pkg}/node_modules" ]; then
        # Lockfiles are gitignored in this repository, so `npm ci` has nothing
        # to install from
        (cd "${pkg}" && npm install --no-audit --no-fund)
    fi
    echo "eslint: ${pkg}"
    (cd "${pkg}" && npm run --silent lint)
done

# The Wish dashboard is the only typed package. Its own build runs tsc as well,
# but that is downstream of this gate.
echo "tsc: addons/selkies-dashboard-wish"
(cd addons/selkies-dashboard-wish && npm run --silent typecheck)
