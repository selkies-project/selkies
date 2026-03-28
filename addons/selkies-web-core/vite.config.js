import { defineConfig } from 'vite';
import envCompatible from 'vite-plugin-env-compatible';
import { ViteMinifyPlugin } from 'vite-plugin-minify';
import ViteRestart from 'vite-plugin-restart'

export default defineConfig({
  base: '',
  server: {
    host: '0.0.0.0',
    allowedHosts: true,
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
})
