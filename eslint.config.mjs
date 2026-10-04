/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The scripts outside the web packages: the Node audits and probes under
// tests/tools, the site's build scripts and the image tier's tester pages. They
// lint with the classic dashboard's ESLint, as addons/eslint.config.mjs does.
import { createRequire } from 'node:module'

const require = createRequire(new URL('addons/selkies-dashboard/', import.meta.url))
const js = require('@eslint/js')
const globals = require('globals')

const rules = {
  ...js.configs.recommended.rules,
  'no-empty': ['error', { allowEmptyCatch: true }],
  'no-unused-vars': ['error', { caughtErrors: 'none' }],
}

export default [
  {
    // The audits run in Node over the web core's browser modules, stubbing
    // the browser globals those touch.
    files: ['tests/tools/**/*.mjs'],
    languageOptions: { ecmaVersion: 'latest', sourceType: 'module', globals: { ...globals.node, ...globals.browser } },
    rules,
  },
  {
    files: ['website/scripts/**/*.mjs'],
    languageOptions: { ecmaVersion: 'latest', sourceType: 'module', globals: globals.node },
    rules,
  },
  {
    // Classic scripts the tester pages load with <script>.
    files: ['tests/image/site/**/*.js'],
    languageOptions: { ecmaVersion: 'latest', sourceType: 'script', globals: globals.browser },
    rules,
  },
]
