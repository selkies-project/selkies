// Prebuild: take the streaming core from selkies-web-core's build into public/,
// which vite serves at the site root where index.html loads it from. Copied at
// build time rather than committed so this dashboard cannot drift behind the
// core the rest of the project ships.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const src = path.resolve(here, '../selkies-web-core/dist/selkies-core.js');
const dst = path.resolve(here, 'public/selkies-core.js');

if (!fs.existsSync(src)) {
  // Unlike the gamepad DB this is not a degradation: without the core the
  // dashboard has no streaming client at all, so stop rather than emit a dist
  // that looks complete and fails in the browser.
  console.error(
    `ERROR: ${src} missing — build selkies-web-core first ` +
    '(npm --prefix ../selkies-web-core run build, or scripts/ci/build-web.sh).'
  );
  process.exit(1);
}
fs.mkdirSync(path.dirname(dst), { recursive: true });
fs.copyFileSync(src, dst);
console.log(`core: ${(fs.statSync(dst).size / 1024).toFixed(0)} KiB copied into public/`);
