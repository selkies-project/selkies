/**
 * Deletes the second copy of every page's payload from the static export.
 *
 * Next.js writes each page's full React Server Components payload twice: as
 * `<page>/index.txt`, which the client fetches to navigate to the page, and as
 * the `/_full` segment `<page>/__next._full.txt`, which only a server answering
 * segment requests by header would serve. The client of a static export never
 * names `_full` (its segment requests are `_tree` and the segments
 * themselves), so the copy is as much dead weight as the payload is large.
 *
 * Each copy is deleted only where it is byte-identical to the page's
 * `index.txt`; one that differs, or a page without `index.txt`, fails the
 * build instead, so a Next.js release that gives `_full` a meaning of its own
 * is caught here rather than shipped broken.
 */
import { readFile, readdir, rm } from 'node:fs/promises';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const FULL = '__next._full.txt';
const out = process.argv[2] ?? join(dirname(dirname(fileURLToPath(import.meta.url))), 'out');

async function* payloads(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* payloads(path);
    else if (entry.name === FULL) yield path;
  }
}

const differing = [];
let dropped = 0;
for await (const full of payloads(out)) {
  const page = join(dirname(full), 'index.txt');
  const [a, b] = await Promise.all([readFile(full), readFile(page).catch(() => undefined)]);
  if (!b || !a.equals(b)) {
    differing.push(relative(out, full));
    continue;
  }
  await rm(full);
  dropped += 1;
}
if (differing.length) {
  console.error(`drop-full-payloads: these differ from their page's index.txt, so Next.js serves them for something else:\n  ${differing.join('\n  ')}`);
  process.exit(1);
}
console.log(`drop-full-payloads: ${dropped} copies of a page payload deleted`);
