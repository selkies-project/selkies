/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The web core and the touch gamepad have no dev dependencies of their own:
// they lint with the dashboard's ESLint, and these packages resolve
// from its node_modules (scripts/ci/lint-web.sh installs them).
import { createRequire } from 'node:module'

const require = createRequire(new URL('selkies-dashboard/', import.meta.url))
const js = require('@eslint/js') as typeof import('@eslint/js')
const globals = require('globals') as typeof import('globals')
const tseslint = require('typescript-eslint') as typeof import('typescript-eslint')

const rules = {
  ...js.configs.recommended.rules,
  // Teardown and capability probes are best-effort: a catch that drops its
  // error says so by being empty.
  'no-empty': ['error', { allowEmptyCatch: true }],
  'no-unused-vars': ['error', { caughtErrors: 'none' }],
}

// typescript-eslint parses through the TypeScript 6 JS API, which the
// `typescript` package here is (the @typescript/typescript6 shim); `tsc` is
// the native TypeScript 7 compiler from @typescript/native. See package.json.
const tsRules = {
  ...js.configs.recommended.rules,
  ...tseslint.configs.recommended.reduce((acc, c) => ({ ...acc, ...c.rules }), {}),
  'no-empty': ['error', { allowEmptyCatch: true }],
  'no-unused-vars': 'off',
  '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none' }],
  // No `any`: a value of unknown shape is `unknown` and narrowed, and the
  // type-checked rules refuse the `any` the DOM and JSON still hand over.
  '@typescript-eslint/no-explicit-any': 'error',
  '@typescript-eslint/no-unsafe-argument': 'error',
  '@typescript-eslint/no-unsafe-assignment': 'error',
  '@typescript-eslint/no-unsafe-call': 'error',
  '@typescript-eslint/no-unsafe-member-access': 'error',
  '@typescript-eslint/no-unsafe-return': 'error',
  // The `.js` files were never held to these three style rules, which the
  // typescript-eslint preset adds; the conversion keeps each declaration as it was.
  'no-var': 'off',
  'prefer-const': 'off',
  'prefer-spread': 'off',
}

export default [
  // Build output.
  { ignores: ['**/dist/'] },
  {
    files: ['selkies-web-core/**/*.{js,mjs}'],
    languageOptions: { ecmaVersion: 'latest', sourceType: 'module', globals: globals.browser },
    rules,
  },
  {
    files: ['selkies-web-core/**/*.ts'],
    ignores: ['selkies-web-core/clipboard-worker.ts'],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
      parser: tseslint.parser,
      parserOptions: {
        ecmaFeatures: { jsx: true },
        sourceType: 'module',
        project: ['selkies-web-core/tsconfig.json'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: tsRules,
  },
  {
    files: ['selkies-web-core/clipboard-worker.ts'],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.worker,
      parser: tseslint.parser,
      parserOptions: {
        ecmaFeatures: { jsx: true },
        sourceType: 'module',
        project: ['selkies-web-core/tsconfig.json'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: tsRules,
  },
  {
    // Build helpers Node runs and this config, held to the type-checked rules
    // through the dashboard's project for such scripts.
    files: ['selkies-web-core/vite.config.ts', 'selkies-web-core/gendb.ts', 'eslint.config.mts'],
    languageOptions: {
      ecmaVersion: 'latest',
      globals: globals.node,
      parser: tseslint.parser,
      parserOptions: {
        sourceType: 'module',
        project: ['selkies-dashboard/tsconfig.node.json'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...js.configs.recommended.rules,
      ...tseslint.configs.recommendedTypeChecked.reduce((acc, c) => ({ ...acc, ...c.rules }), {}),
      'no-empty': ['error', { allowEmptyCatch: true }],
      '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none' }],
    },
  },
  {
    // Typed throughout and held to the type-checked rules, through the
    // dashboard's project that compiles it: a message payload is `unknown`
    // until narrowed.
    files: ['universal-touch-gamepad/**/*.ts'],
    languageOptions: {
      ecmaVersion: 'latest',
      globals: globals.browser,
      parser: tseslint.parser,
      parserOptions: {
        sourceType: 'module',
        project: ['selkies-dashboard/tsconfig.app.json'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...js.configs.recommended.rules,
      ...tseslint.configs.recommendedTypeChecked.reduce((acc, c) => ({ ...acc, ...c.rules }), {}),
      'no-empty': ['error', { allowEmptyCatch: true }],
      '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none' }],
    },
  },
]
