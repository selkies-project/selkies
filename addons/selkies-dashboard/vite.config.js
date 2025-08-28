import { defineConfig, loadEnv } from 'vite'
import react from '@vitejs/plugin-react'
import ViteRestart from 'vite-plugin-restart'
import { ViteMinifyPlugin } from 'vite-plugin-minify';

export default ({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  const inject = env.SELKIES_INJECT === '1' || env.SELKIES_INJECT === 'true';
  const selkiesMode = env.SELKIES_MODE || 'websockets';
  const downloadsPath = env.SELKIES_UPLOAD_DIR || '~/Desktop';

  return defineConfig({
    base: '',
    server: {
      host: '0.0.0.0',
      allowedHosts: true,
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
      'window.__SELKIES_STREAMING_MODE__': inject ? JSON.stringify(selkiesMode) : 'undefined',
      'window.__SELKIES_INJECTED_PATH_PREFIX__': inject ? JSON.stringify(downloadsPath) : 'undefined'
    }
  })
};