/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
// typescript-eslint parses through the TypeScript 6 JS API, which the
// `typescript` package here is (the @typescript/typescript6 shim); `tsc` is
// the native TypeScript 7 compiler from @typescript/native. See package.json.
import tseslint from 'typescript-eslint'

// The UI is TypeScript (src/**/*.{ts,tsx}); the plain-JS entries are the
// bootstrap (src/main.jsx) and the node-side build scripts. Both get the same
// react-hooks and react-refresh rules; the TS block swaps in the typed parser
// and typescript-eslint's recommended set.
const reactRules = {
  ...reactHooks.configs.recommended.rules,
  'react-refresh/only-export-components': [
    'warn',
    { allowConstantExport: true },
  ],
}

export default [
  // public/selkies-core.js is the bundled streaming core, not source.
  { ignores: ['dist', 'public/selkies-core.js'] },
  {
    files: ['*.config.js', 'copy-*.js'],
    languageOptions: { globals: globals.node },
  },
  {
    files: ['**/*.{js,jsx}'],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
      parserOptions: {
        ecmaVersion: 'latest',
        ecmaFeatures: { jsx: true },
        sourceType: 'module',
      },
    },
    plugins: {
      'react-hooks': reactHooks,
      'react-refresh': reactRefresh,
    },
    rules: {
      ...js.configs.recommended.rules,
      ...reactRules,
      // Count JSX element names as references so no-unused-vars can safely
      // exempt only ALL_CAPS constants instead of everything PascalCase
      // (which let unused component and React imports go unflagged).
      'no-unused-vars': ['error', { varsIgnorePattern: '^[A-Z_][A-Z0-9_]*$' }],
    },
  },
  {
    files: ['**/*.{ts,tsx}'],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
      parser: tseslint.parser,
      parserOptions: {
        ecmaFeatures: { jsx: true },
        sourceType: 'module',
      },
    },
    plugins: {
      '@typescript-eslint': tseslint.plugin,
      'react-hooks': reactHooks,
      'react-refresh': reactRefresh,
    },
    rules: {
      ...js.configs.recommended.rules,
      ...tseslint.configs.recommended.reduce((acc, c) => ({ ...acc, ...c.rules }), {}),
      ...reactRules,
      // The server-settings document and the core's postMessage payloads are
      // untyped JSON; `any` is their honest type here.
      '@typescript-eslint/no-explicit-any': 'off',
    },
  },
  {
    // shadcn-managed primitives export their variant helpers and hooks beside
    // the component by design (components.json owns this directory).
    files: ['src/components/ui/**/*.{ts,tsx}'],
    rules: { 'react-refresh/only-export-components': 'off' },
  },
]
