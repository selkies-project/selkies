/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import path from "path"
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig, loadEnv } from "vite"
import { ViteMinifyPlugin } from "vite-plugin-minify"

// https://vite.dev/config/
export default ({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  const inject = env.SELKIES_INJECT === '1' || env.SELKIES_INJECT === 'true';
  const downloadsPath = env.SELKIES_UPLOAD_DIR || '~/Desktop';

  return defineConfig({
    // Relative asset URLs so the dist works when served under a subfolder prefix.
    base: '',
    plugins: [react(), tailwindcss(), ViteMinifyPlugin()],
    resolve: {
      alias: {
        "@": path.resolve(__dirname, "./src"),
      },
    },
    server: {
      host: "0.0.0.0",
      allowedHosts: ['.trycloudflare.com'],
      // main.jsx imports the touch-gamepad addon from its sibling package.
      fs: { allow: ['.', '../universal-touch-gamepad'] },
    },
    build: {
      target: 'chrome94',
      chunkSizeWarningLimit: 1000,
      rollupOptions: {
        output: {
          manualChunks: (id) => {
            if (id.includes('node_modules')) {
              if (id.includes('@radix-ui') || id.includes('radix-ui')) {
                return 'radix-ui';
              }
              if (id.includes('react') || id.includes('framer-motion') || id.includes('lucide-react')) {
                return 'vendor';
              }
            }
          }
        }
      }
    },
    define: {
      // if inject=false -> undefined, so runtime falls back to localStorage/default
      'window.__SELKIES_INJECTED_PATH_PREFIX__': inject ? JSON.stringify(downloadsPath) : 'undefined'
    }
  })
}
