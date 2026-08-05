/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { defineConfig, loadEnv } from 'vite'
import react from '@vitejs/plugin-react'
import { ViteMinifyPlugin } from 'vite-plugin-minify';

// Restarts the dev server when a file Vite does not track as a module changes.
function restartOnChange(globs) {
  const patterns = globs.map((glob) => new RegExp(
    '(^|/)' + glob.replace(/[.+^${}()|[\]\\]/g, '\\$&')
                  .replace(/\*\*/g, ' ')
                  .replace(/\*/g, '[^/]*')
                  .replace(/ /g, '.*') + '$'));
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
      // main.jsx imports the touch-gamepad addon from its sibling package.
      fs: { allow: ['.', '../universal-touch-gamepad'] },
    },
    build: {
      target: 'chrome94'
    },
    plugins: [
      react({
        exclude: 'src/selkies-core.js'
      }),
      ViteMinifyPlugin(),
      restartOnChange(['index.html', 'src/**']),
    ],
    define: {
      // if inject=false -> undefined, so runtime falls back to localStorage/default
      'window.__SELKIES_INJECTED_PATH_PREFIX__': inject ? JSON.stringify(downloadsPath) : 'undefined'
    }
  })
};