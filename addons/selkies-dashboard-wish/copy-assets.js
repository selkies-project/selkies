// Postbuild: ship what selkies-web-core's build leaves beside its bundle into
// this dashboard's dist: the gamepad remap DB (its postbuild gendb.js) and
// libopus in WASM (its vite build), each fetched relative to the page. Built
// at build time so neither lives in the repo.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const core = path.resolve(here, '../selkies-web-core/dist');

const ASSETS = [
  ['jsdb', 'gamepad remap lookups (jsdb/<platform>/*.json) will 404 and pads fall back to browser-default mappings'],
  ['codecs', 'engines without WebCodecs audio will find no Opus decoder or encoder (codecs/) and play and send no sound'],
];

for (const [name, without] of ASSETS) {
  const src = path.join(core, name);
  const dst = path.resolve(here, 'dist', name);
  if (!fs.existsSync(src)) {
    console.warn(`WARNING: ${src} missing — build selkies-web-core first. Without it ${without}.`);
    continue;
  }
  fs.rmSync(dst, { recursive: true, force: true });
  fs.cpSync(src, dst, { recursive: true });
  const files = fs.readdirSync(dst, { recursive: true, withFileTypes: true }).filter((entry) => entry.isFile()).length;
  console.log(`${name}: ${files} files copied into dist/`);
}
