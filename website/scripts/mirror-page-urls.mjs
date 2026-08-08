// Publishes every page at both /page and /page/.
//
// The export writes one file per page, which GitHub Pages serves for the
// slashless URL and 404s for the other. Writing the same document to
// <page>/index.html as well makes both spellings resolve directly, so neither
// is the one a link has to use.
import { copyFile, mkdir, readdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const out = process.argv[2] ?? join(dirname(dirname(fileURLToPath(import.meta.url))), 'out');

async function mirror(dir) {
  let written = 0;
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      written += await mirror(path);
    } else if (entry.name.endsWith('.html') && entry.name !== 'index.html') {
      const asDirectory = path.slice(0, -'.html'.length);
      await mkdir(asDirectory, { recursive: true });
      await copyFile(path, join(asDirectory, 'index.html'));
      written += 1;
    }
  }
  return written;
}

console.log(`mirror-page-urls: ${await mirror(out)} pages also written with a trailing slash`);
