/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The web core and the touch gamepad have no dev dependencies of their own:
// they lint with the classic dashboard's ESLint, and these packages resolve
// from its node_modules (scripts/ci/lint-web.sh installs them).
import { createRequire } from 'node:module'

const require = createRequire(new URL('selkies-dashboard/', import.meta.url))
const js = require('@eslint/js')
const globals = require('globals')

const rules = {
  ...js.configs.recommended.rules,
  // Teardown and capability probes are best-effort: a catch that drops its
  // error says so by being empty.
  'no-empty': ['error', { allowEmptyCatch: true }],
  'no-unused-vars': ['error', { caughtErrors: 'none' }],
}

export default [
  // Build output.
  { ignores: ['**/dist/'] },
  {
    files: ['selkies-web-core/**/*.{js,mjs}'],
    ignores: ['selkies-web-core/clipboard-worker.js'],
    languageOptions: { ecmaVersion: 'latest', sourceType: 'module', globals: globals.browser },
    rules,
  },
  {
    files: ['selkies-web-core/clipboard-worker.js'],
    languageOptions: { ecmaVersion: 'latest', sourceType: 'module', globals: globals.worker },
    rules,
  },
  {
    files: ['selkies-web-core/vite.config.mjs', 'selkies-web-core/gendb.js'],
    languageOptions: { globals: globals.node },
  },
  {
    // Classic scripts: the gamepad's own page loads them with <script>.
    files: ['universal-touch-gamepad/**/*.js'],
    languageOptions: { ecmaVersion: 'latest', sourceType: 'script', globals: globals.browser },
    rules,
  },
]
