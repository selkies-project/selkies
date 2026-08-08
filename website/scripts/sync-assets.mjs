// Mirrors docs/assets into public/ so the images the Markdown references
// resolve on the site. The files stay under docs/ because that is where GitHub
// renders and edits them; Next.js only serves what is under public/.
import { cp, rm } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const website = dirname(dirname(fileURLToPath(import.meta.url)));
const from = join(website, '..', 'docs', 'assets');
const to = join(website, 'public', 'assets');

await rm(to, { recursive: true, force: true });
await cp(from, to, { recursive: true });
console.log(`sync-assets: ${from} -> ${to}`);
