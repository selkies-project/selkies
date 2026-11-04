/**
 * Builds docs/reference, the Developer Reference section of the site, from
 * the predev/prebuild npm hooks: as pages of the site, or as one page linking
 * the download an older release publishes it as.
 *
 * A reference rendered as pages is nearly all of a version's size, a few
 * hundred megabytes beside a few of prose, so the published site carries it
 * as pages for the branch and the latest release alone; an older release's
 * is a file attached to that release (build-reference-archive.mjs), which
 * opens offline. build-versions.mjs names it through two variables:
 * `REFERENCE_FROM=release` and `REFERENCE_DOWNLOAD`, the file's address.
 *
 * Without `REFERENCE_FROM` the reference is pages, as the branch, the latest
 * release, `npm run build`, and `npm run dev` render it: the server's half
 * from generate-rust-docs.mjs (or generate-python-docs.mjs for a tree whose
 * server is the Python package) and the web client's from
 * generate-web-docs.mjs, run in that order since the web half appends to the
 * sidebar entry the server's half writes.
 */
import { existsSync } from 'node:fs';
import * as fs from 'node:fs/promises';
import { basename, join } from 'node:path';
import { repoRoot, run, site } from './version-tree.mjs';

const outDir = join(repoRoot, 'docs', 'reference');
const from = process.env.REFERENCE_FROM;
const download = process.env.REFERENCE_DOWNLOAD;

/** What the archive holds, by what this tree's server is written in. */
function halves() {
  const server = existsSync(join(repoRoot, 'crates', 'selkies', 'Cargo.toml'))
    ? "the server's crates, `selkies` and `selkies-rtc`, as rustdoc renders them, with its search"
    : "the server's Python modules as static pages";
  return `${server}, and the web client core and the dashboard as TypeDoc renders them, with its search`;
}

/** The body of the section's page, for a release whose reference is a download. */
function downloadPage() {
  const file = `[\`${basename(new URL(download).pathname)}\`](${download})`;
  return `The Developer Reference of this release is generated from the docblocks in its code and published as a file attached to the release rather than as pages of this site: ${file}. It opens offline: extract it and open \`index.html\`. It holds ${halves()}.`;
}

if (!from) {
  for (const script of ['generate-python-docs.mjs', 'generate-rust-docs.mjs', 'generate-web-docs.mjs']) {
    run('node', [join(site, 'scripts', script)]);
  }
  process.exit(0);
}
if (from !== 'release') throw new Error(`generate-reference: REFERENCE_FROM is ${from}`);
if (!download) throw new Error('generate-reference: a release names its REFERENCE_DOWNLOAD');

const index = `---
title: Developer Reference
description: The reference generated from the docblocks in the code, as a download.
---

${downloadPage()}

The latest release and the code under development carry their Developer Reference as pages of this site; every other release's is attached to that release, and its version of this site links it.
`;

await fs.rm(join(site, 'public', 'rustdoc'), { recursive: true, force: true });
await fs.rm(outDir, { recursive: true, force: true });
await fs.mkdir(outDir, { recursive: true });
await fs.writeFile(join(outDir, 'index.md'), index);
await fs.writeFile(join(outDir, 'meta.json'), `${JSON.stringify({ title: 'Developer Reference', pages: ['index'] }, null, 2)}\n`);
console.log(`generate-reference: the reference is the download ${download}`);
