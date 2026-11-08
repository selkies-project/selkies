/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import { readFileSync } from 'node:fs';
import { defineConfig, transformWithOxc } from 'vite';
import { ViteMinifyPlugin } from 'vite-plugin-minify';

// selkies-ws-core.ts inlines three lib modules into Blob workers by source
// (`?raw`): a bundler rename would leave the stringified copy's bindings
// unresolved. Vite hands back the raw file, so a TypeScript source would
// land in the worker verbatim and fail to parse; run it through the OXC
// transform and hand the worker the emitted JavaScript instead. Only those
// three are inlined this way.
const RAW_TS_MODULES = new Set([
  'lib/decode-gate.ts',
  'lib/decode-pace.ts',
  'lib/wire-codecs.ts',
]);
function stripRawTs() {
  return {
    name: 'selkies-strip-raw-ts',
    enforce: 'pre',
    async load(id) {
      if (!id.includes('?raw')) return;
      const file = id.split('?')[0];
      const rel = file.split(/[\\/]/).join('/').replace(/.*selkies-web-core\//, '');
      if (!RAW_TS_MODULES.has(rel)) return;
      const source = readFileSync(file, 'utf8');
      const { code } = await transformWithOxc(source, file);
      return { code: `export default ${JSON.stringify(code)};`, map: null };
    },
  };
}

// libopus in WASM for the engines without WebCodecs audio, served from codecs/
// beside the bundle for them to import on first use; no other engine loads it.
// The generated module is renamed to .js, since a module script needs a
// JavaScript type and servers type .mjs unevenly.
function opusWasm() {
  const pkg = new URL('./node_modules/libopus-wasm/', import.meta.url);
  const GENERATED = '"./generated/libopus.generated.mjs"';
  const files = () => {
    const index = readFileSync(new URL('dist/index.js', pkg), 'utf8');
    if (!index.includes(GENERATED)) throw new Error(`libopus-wasm's index.js no longer imports ${GENERATED}`);
    return {
      'codecs/libopus-wasm/index.js': index.replace(GENERATED, '"./libopus.generated.js"'),
      'codecs/libopus-wasm/libopus.generated.js': readFileSync(new URL('dist/generated/libopus.generated.mjs', pkg)),
      'codecs/libopus-wasm/LICENSE': readFileSync(new URL('LICENSE', pkg)),
      'codecs/libopus-wasm/THIRD_PARTY_NOTICES.md': readFileSync(new URL('THIRD_PARTY_NOTICES.md', pkg)),
    };
  };
  return {
    name: 'selkies-opus-wasm',
    configureServer(server) {
      const served = files();
      server.middlewares.use((req, res, next) => {
        const name = (req.url || '').split('?')[0].replace(/^\//, '');
        if (!served[name]) return next();
        res.setHeader('Content-Type', name.endsWith('.js') ? 'text/javascript' : 'text/plain');
        res.end(served[name]);
      });
    },
    generateBundle() {
      for (const [fileName, source] of Object.entries(files())) this.emitFile({ type: 'asset', fileName, source });
    },
  };
}

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
    opusWasm(),
    stripRawTs(),
    restartOnChange(['selkies-core.ts', 'lib/**', 'selkies-version.txt']),
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
