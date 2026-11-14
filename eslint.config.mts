/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The scripts outside the web packages: the Node audits and probes under
// tests/tools, the BrowserStack smoke, the site's build scripts and the image
// tier's tester pages. They lint with the dashboard's ESLint and
// typescript-eslint, as web/eslint.config.mts does.
import { createRequire } from 'node:module'

const require = createRequire(new URL('web/selkies-dashboard/', import.meta.url))
const js = require('@eslint/js') as typeof import('@eslint/js')
const globals = require('globals') as typeof import('globals')
const tseslint = require('typescript-eslint') as typeof import('typescript-eslint')

const rules = {
  ...js.configs.recommended.rules,
  'no-empty': ['error', { allowEmptyCatch: true }],
  'no-unused-vars': ['error', { caughtErrors: 'none' }],
}

export default [
  {
    // TypeScript Node runs with the types stripped, held to the type-checked
    // rules through the projects that compile them: the audits, which run the
    // web core's browser modules over stand-ins for the browser globals those
    // touch, the site's build scripts and configs, and this config.
    files: [
      'tests/tools/**/*.ts',
      'website/scripts/**/*.ts',
      'website/next.config.ts',
      'website/postcss.config.ts',
      'eslint.config.mts',
    ],
    languageOptions: {
      ecmaVersion: 'latest',
      sourceType: 'module',
      globals: { ...globals.node, ...globals.browser },
      parser: tseslint.parser,
      parserOptions: {
        project: [
          'tests/tools/tsconfig.json',
          'tests/tools/tsconfig.browser.json',
          'web/selkies-dashboard/tsconfig.node.json',
          'website/tsconfig.json',
        ],
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...rules,
      ...tseslint.configs.recommendedTypeChecked.reduce((acc, c) => ({ ...acc, ...c.rules }), {}),
      'no-unused-vars': 'off',
      '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none' }],
    },
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
    // The classic script the tester pages load with <script>, its types
    // stripped, checked with the browser's types alone.
    files: ['tests/image/site/**/*.ts'],
    languageOptions: {
      ecmaVersion: 'latest',
      sourceType: 'script',
      globals: globals.browser,
      parser: tseslint.parser,
      parserOptions: { project: ['tests/tools/tsconfig.browser.json'], tsconfigRootDir: import.meta.dirname },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...rules,
      ...tseslint.configs.recommendedTypeChecked.reduce((acc, c) => ({ ...acc, ...c.rules }), {}),
      'no-unused-vars': 'off',
      '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none' }],
    },
  },
]
