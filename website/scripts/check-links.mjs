// Walks the exported site and fails on any internal link, image, or anchor
// that does not resolve. Pages cross-reference each other by file name, and
// those names are rewritten at build time, so a rename that breaks a link is
// invisible until something checks the output.
import { readFile, readdir } from 'node:fs/promises';
import { dirname, join, posix, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const out = process.argv[2] ?? join(dirname(dirname(fileURLToPath(import.meta.url))), 'out');
const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? '';

async function walk(dir) {
  const found = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) found.push(...(await walk(path)));
    else found.push(path);
  }
  return found;
}

const files = await walk(out);
const exists = new Set(files.map((f) => `/${relative(out, f).split(/[\\/]/).join('/')}`));
const pages = files.filter((f) => f.endsWith('.html'));

/** Every id an exported page defines, so anchors can be checked against it. */
const ids = new Map();
for (const page of pages) {
  const html = await readFile(page, 'utf8');
  const set = new Set();
  for (const [, id] of html.matchAll(/\sid="([^"]+)"/g)) set.add(id);
  ids.set(`/${relative(out, page).split(/[\\/]/).join('/')}`, set);
}

/** The file a site-absolute URL is served from, or undefined. */
function resolve(path) {
  for (const candidate of [path, `${path}/index.html`, `${path}.html`, `${path.replace(/\/$/, '')}/index.html`]) {
    const normalized = candidate.replace(/\/{2,}/g, '/');
    if (exists.has(normalized)) return normalized;
  }
}

const problems = [];
let checked = 0;

for (const page of pages) {
  const url = `/${relative(out, page).split(/[\\/]/).join('/')}`;
  const html = await readFile(page, 'utf8');
  const here = posix.dirname(url);

  for (const [, attr, raw] of html.matchAll(/\s(href|src)="([^"]*)"/g)) {
    if (!raw || raw.startsWith('#') || /^[a-z][a-z0-9+.-]*:/i.test(raw) || raw.startsWith('//')) continue;
    const [target, hash] = raw.split('#');
    let path = target || url;
    if (!path.startsWith('/')) path = posix.join(here, path);
    else if (basePath) {
      if (!path.startsWith(`${basePath}/`) && path !== basePath) {
        problems.push(`${url}: ${attr} "${raw}" is missing the ${basePath} base path`);
        continue;
      }
      path = path.slice(basePath.length) || '/';
    }

    checked += 1;
    const file = resolve(path);
    if (!file) {
      problems.push(`${url}: ${attr} "${raw}" resolves to nothing`);
      continue;
    }
    if (hash && ids.has(file) && !ids.get(file).has(decodeURIComponent(hash))) {
      problems.push(`${url}: ${attr} "${raw}" has no matching id in ${file}`);
    }
  }
}

for (const problem of problems) console.error(`FAIL ${problem}`);
console.log(
  `check-links: ${checked} internal references over ${pages.length} pages, ${problems.length} broken`,
);
process.exit(problems.length === 0 ? 0 : 1);
