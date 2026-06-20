/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { defineConfig, loadEnv } from 'vite'
import react from '@vitejs/plugin-react'
import ViteRestart from 'vite-plugin-restart'
import { ViteMinifyPlugin } from 'vite-plugin-minify';

export default ({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  const inject = env.SELKIES_INJECT === '1' || env.SELKIES_INJECT === 'true';
  const downloadsPath = env.SELKIES_UPLOAD_DIR || '~/Desktop';

  return defineConfig({
    base: '',
    server: {
      // Dev-server exposure is opt-in: bind loopback unless SELKIES_VITE_HOST is set.
      host: process.env.SELKIES_VITE_HOST || '127.0.0.1',
      allowedHosts: process.env.SELKIES_VITE_HOST ? true : undefined,
    },
    build: {
      target: 'chrome94'
    },
    plugins: [
      react({
        exclude: 'src/selkies-core.js'
      }),
      ViteMinifyPlugin(),
      ViteRestart({restart: ['index.html', 'src/**']}),
    ],
    define: {
      // if inject=false -> undefined, so runtime falls back to localStorage/default
      'window.__SELKIES_INJECTED_PATH_PREFIX__': inject ? JSON.stringify(downloadsPath) : 'undefined'
    }
  })
};