/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { defineConfig } from 'vite';
import envCompatible from 'vite-plugin-env-compatible';
import { ViteMinifyPlugin } from 'vite-plugin-minify';
import ViteRestart from 'vite-plugin-restart'

export default defineConfig({
  base: '',
  server: {
    // Dev-server exposure is opt-in: bind loopback unless SELKIES_VITE_HOST is set
    // (e.g. SELKIES_VITE_HOST=0.0.0.0 for LAN testing). Vite restricts allowed hosts
    // to loopback by default; wide binding also opts into allowing all hosts.
    host: process.env.SELKIES_VITE_HOST || '127.0.0.1',
    allowedHosts: process.env.SELKIES_VITE_HOST ? true : undefined,
  },
  plugins: [
    envCompatible(),
    ViteMinifyPlugin(),
    ViteRestart({restart: ['selkies-core.js', 'lib/**','selkies-version.txt']}),
  ],
  build: {
    target: 'chrome94',
    rollupOptions: {
      input: {
        main: './index.html',
      },
      output: {
        entryFileNames: 'selkies-core.js'
      }
    }
  },
  worker: {
    format: 'es',
    rollupOptions: {
      output: {
        entryFileNames: '[name]-[hash].js'
      }
    }
  }
})
