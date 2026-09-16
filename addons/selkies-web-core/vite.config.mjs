/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { defineConfig } from 'vite';
import { ViteMinifyPlugin } from 'vite-plugin-minify';

// Restarts the dev server when a file Vite does not track as a module changes.
function restartOnChange(globs) {
  const patterns = globs.map((glob) => new RegExp(
    '(^|/)' + glob.replace(/[.+^${}()|[\]\\]/g, '\\$&')
                  .replace(/\*\*/g, '\0')
                  .replace(/\*/g, '[^/]*')
                  .replace(/\0/g, '.*') + '$'));
  return {
    name: 'selkies-restart-on-change',
    apply: 'serve',
    configureServer(server) {
      server.watcher.add(globs);
      const onChange = (file) => {
        const path = file.split(/[\\/]/).join('/');
        if (patterns.some((pattern) => pattern.test(path))) server.restart();
      };
      server.watcher.on('add', onChange);
      server.watcher.on('change', onChange);
    },
  };
}

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
    ViteMinifyPlugin(),
    restartOnChange(['selkies-core.js', 'lib/**', 'selkies-version.txt']),
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
