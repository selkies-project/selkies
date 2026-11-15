/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The web core and the touch gamepad have no dev dependencies of their own:
// they lint with the dashboard's ESLint, and these packages resolve
// from its node_modules (scripts/ci/lint-web.sh installs them).
import { createRequire } from 'node:module'
import { fields } from './selkies-web-core/lib/narrow.ts'

const require = createRequire(new URL('selkies-dashboard/', import.meta.url))
// Loaded by name from the dashboard's node_modules, which no import here can
// name, so each package is read for the members this config takes from it.
const js = fields(require('@eslint/js'))
const globals = fields(require('globals'))
const tseslint = fields(require('typescript-eslint'))
/** The rules a preset turns on, a config or a list of them. */
const rulesOf = (preset: unknown): Record<string, unknown> =>
  (Array.isArray(preset) ? preset : [preset]).reduce<Record<string, unknown>>((acc, c) => ({ ...acc, ...fields(fields(c).rules) }), {})
/** One of the `globals` package's tables, by environment. */
const globalsOf = (name: string): Record<string, unknown> => fields(globals[name])
const jsRecommended = rulesOf(fields(js.configs).recommended)
const tsRecommended = rulesOf(fields(tseslint.configs).recommended)
// The type-checked preset, and no type assertion that narrows: a check narrows, never `as`.
const tsTypeChecked = { ...rulesOf(fields(tseslint.configs).recommendedTypeChecked), '@typescript-eslint/no-unsafe-type-assertion': 'error' }

const rules = {
  ...jsRecommended,
  // Teardown and capability probes are best-effort: a catch that drops its
  // error says so by being empty.
  'no-empty': ['error', { allowEmptyCatch: true }],
  'no-unused-vars': ['error', { caughtErrors: 'none' }],
}

// typescript-eslint parses through the TypeScript 6 JS API, which the
// `typescript` package here is (the @typescript/typescript6 shim); `tsc` is
// the native TypeScript 7 compiler from @typescript/native. See package.json.
const tsRules = {
  ...jsRecommended,
  ...tsRecommended,
  'no-empty': ['error', { allowEmptyCatch: true }],
  'no-unused-vars': 'off',
  '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none' }],
  // No `any`: a value of unknown shape is `unknown` and narrowed, and the
  // type-checked rules refuse the `any` the DOM and JSON still hand over.
  // Nor a type assertion that narrows: a check narrows, never `as`.
  '@typescript-eslint/no-explicit-any': 'error',
  '@typescript-eslint/no-unsafe-argument': 'error',
  '@typescript-eslint/no-unsafe-assignment': 'error',
  '@typescript-eslint/no-unsafe-call': 'error',
  '@typescript-eslint/no-unsafe-member-access': 'error',
  '@typescript-eslint/no-unsafe-return': 'error',
  '@typescript-eslint/no-unsafe-type-assertion': 'error',
  // The `.js` files were never held to these three style rules, which the
  // typescript-eslint preset adds; the conversion keeps each declaration as it was.
  'no-var': 'off',
  'prefer-const': 'off',
  'prefer-spread': 'off',
}

export default [
  // A rule holds everywhere it applies: no comment turns one off or
  // reconfigures it, so a finding is fixed in the code.
  { linterOptions: { noInlineConfig: true } },
  // Build output.
  { ignores: ['**/dist/'] },
  {
    files: ['selkies-web-core/**/*.{js,mjs}'],
    languageOptions: { ecmaVersion: 'latest', sourceType: 'module', globals: globalsOf('browser') },
    rules,
  },
  {
    files: ['selkies-web-core/**/*.ts'],
    ignores: ['selkies-web-core/clipboard-worker.ts'],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globalsOf('browser'),
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
      globals: globalsOf('worker'),
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
      globals: globalsOf('node'),
      parser: tseslint.parser,
      parserOptions: {
        sourceType: 'module',
        project: ['selkies-dashboard/tsconfig.node.json'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...jsRecommended,
      ...tsTypeChecked,
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
      globals: globalsOf('browser'),
      parser: tseslint.parser,
      parserOptions: {
        sourceType: 'module',
        project: ['selkies-dashboard/tsconfig.app.json'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...jsRecommended,
      ...tsTypeChecked,
      'no-empty': ['error', { allowEmptyCatch: true }],
      '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none' }],
    },
  },
]
