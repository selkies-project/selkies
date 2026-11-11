/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The scripts outside the web packages: the Node audits and probes under
// tests/tools, the BrowserStack smoke, the site's build scripts and the image
// tier's tester pages. They lint with the dashboard's ESLint and
// typescript-eslint, as web/eslint.config.mjs does.
import { createRequire } from 'node:module'

const require = createRequire(new URL('web/selkies-dashboard/', import.meta.url))
const js = require('@eslint/js')
const globals = require('globals')
const tseslint = require('typescript-eslint')

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
    // Node programs; the browser-side checks they run are strings.
    files: ['tests/browserstack/**/*.ts'],
    ignores: ['tests/browserstack/node_modules/'],
    languageOptions: { ecmaVersion: 'latest', sourceType: 'module', globals: globals.node, parser: tseslint.parser },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...rules,
      ...tseslint.configs.recommended.reduce((acc, c) => ({ ...acc, ...c.rules }), {}),
      'no-unused-vars': 'off',
      '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none' }],
    },
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
