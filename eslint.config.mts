/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The scripts outside the web packages: the Node audits and probes under
// tests/tools, the browser suites and their library, the BrowserStack smoke, the site's build scripts and the image
// tier's tester pages. They lint with the dashboard's ESLint and
// typescript-eslint, as web/eslint.config.mts does.
import { createRequire } from 'node:module'
import { fields } from './web/selkies-web-core/lib/narrow.ts'

const require = createRequire(new URL('web/selkies-dashboard/', import.meta.url))
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
// The type-checked preset, and no type assertion that narrows: a check narrows, never `as`.
const tsTypeChecked = { ...rulesOf(fields(tseslint.configs).recommendedTypeChecked), '@typescript-eslint/no-unsafe-type-assertion': 'error' }

const rules = {
  ...jsRecommended,
  'no-empty': ['error', { allowEmptyCatch: true }],
  'no-unused-vars': ['error', { caughtErrors: 'none' }],
}

export default [
  // A rule holds everywhere it applies: no comment turns one off or
  // reconfigures it, so a finding is fixed in the code.
  { linterOptions: { noInlineConfig: true } },
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
      globals: { ...globalsOf('node'), ...globalsOf('browser') },
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
      ...tsTypeChecked,
      'no-unused-vars': 'off',
      '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none' }],
    },
  },
  {
    // The browser suites on Playwright Test and their library: Node programs
    // whose page callbacks run in the browser, checked against the
    // Playwright and Node types the tests package carries.
    files: ['tests/lib/**/*.ts', 'tests/integration/**/*.spec.ts', 'tests/e2e/**/*.spec.ts', 'tests/playwright.config.ts'],
    languageOptions: {
      ecmaVersion: 'latest',
      sourceType: 'module',
      globals: { ...globalsOf('node'), ...globalsOf('browser') },
      parser: tseslint.parser,
      parserOptions: { project: ['tests/tsconfig.json'], tsconfigRootDir: import.meta.dirname },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...rules,
      ...tsTypeChecked,
      'no-unused-vars': 'off',
      '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none' }],
    },
  },
  {
    // Node programs; the browser-side checks they run are strings.
    files: ['tests/browserstack/**/*.ts'],
    ignores: ['tests/browserstack/node_modules/'],
    languageOptions: {
      ecmaVersion: 'latest',
      sourceType: 'module',
      globals: globalsOf('node'),
      parser: tseslint.parser,
      parserOptions: { project: ['tests/browserstack/tsconfig.json'], tsconfigRootDir: import.meta.dirname },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...rules,
      ...tsTypeChecked,
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
      globals: globalsOf('browser'),
      parser: tseslint.parser,
      parserOptions: { project: ['tests/tools/tsconfig.browser.json'], tsconfigRootDir: import.meta.dirname },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...rules,
      ...tsTypeChecked,
      'no-unused-vars': 'off',
      // IT is the global the tester pages call.
      '@typescript-eslint/no-unused-vars': ['error', { caughtErrors: 'none', varsIgnorePattern: '^IT$' }],
    },
  },
]
