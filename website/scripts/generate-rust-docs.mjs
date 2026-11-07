/**
 * Builds the server half of the Developer Reference from the rustdoc
 * docblocks of the workspace under crates/.
 *
 * `cargo doc` renders all four public workspace crates, private items
 * included, since the reference covers what the docblocks cover, as it does
 * for the web client. rustdoc's pages are served as rustdoc wrote them, with
 * their own search and source view, rather than converted into site pages:
 * they are copied into public/rustdoc, which the static export serves beside
 * the pages, and the reference's index page and sidebar link into them. Both
 * are build output, not source: public/rustdoc and docs/reference are
 * gitignored and this script runs from the predev/prebuild npm hooks, so a
 * local build and the Pages workflow both generate them fresh from the code.
 *
 * `cargo doc` type-checks every dependency, which runs the build scripts of
 * pixelflux and pcmflux, so it needs the Rust toolchain and the codec, audio,
 * and display libraries they build against (the packages the `server` stage
 * of addons/base/Dockerfile installs). It builds in a target directory of its
 * own, `reference` under the workspace's (`CARGO_TARGET_DIR` is honored), so
 * the rustdoc output holds these crates alone whatever else was documented in
 * the workspace's. A tree without the workspace, a release whose server is
 * the Python package under src/selkies, is left to generate-python-docs.mjs.
 *
 * `--archive <dir>` writes the rustdoc tree into `<dir>` instead, for the
 * download build-reference-archive.mjs attaches to a release, and leaves the
 * site's pages alone. rustdoc links its pages relatively and loads its search
 * index by script tag, so the copy opens from a file as it does from the site.
 * That build has no earlier reference to fall back on and fails outright.
 */
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import * as fs from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const repoRoot = join(root, '..');
const outDir = join(repoRoot, 'docs', 'reference');
const rustdocDir = join(root, 'public', 'rustdoc');
const workspaceTarget = process.env.CARGO_TARGET_DIR ? resolve(repoRoot, process.env.CARGO_TARGET_DIR) : join(repoRoot, 'target');
const targetDir = join(workspaceTarget, 'reference');

// Each crate as its package is named and as rustdoc names its directory.
const CRATES = [
  { pkg: 'selkies', dir: 'selkies', summary: 'the streaming server and the `selkies` executable' },
  { pkg: 'selkies-rtc', dir: 'selkies_rtc', summary: 'its WebRTC stack' },
  { pkg: 'pixelflux', dir: 'pixelflux', summary: 'screen capture and video encoding' },
  { pkg: 'pcmflux', dir: 'pcmflux', summary: 'audio capture, playback, and Opus encoding' },
];

if (!existsSync(join(repoRoot, 'crates', 'selkies', 'Cargo.toml'))) {
  console.log('generate-rust-docs: no Rust workspace in this tree; skipped');
  process.exit(0);
}

function cargoDoc() {
  const packages = CRATES.flatMap((c) => ['-p', c.pkg]);
  const args = ['doc', '--no-deps', '--lib', '--locked', '--document-private-items', '--target-dir', targetDir, ...packages];
  const res = spawnSync('cargo', args, { cwd: repoRoot, stdio: 'inherit' });
  if (res.status !== 0) {
    throw new Error(`cargo ${args.join(' ')} exited with ${res.status ?? res.error}`);
  }
}

const archiveAt = process.argv.indexOf('--archive');
const archiveDir = archiveAt === -1 ? undefined : resolve(process.argv[archiveAt + 1] ?? '');

/** Copies rustdoc's output to `dir`, with a page at the documentation root. */
async function copyRustdoc(dir) {
  await fs.rm(dir, { recursive: true, force: true });
  // rustdoc's build lock is not part of the output.
  await fs.cp(join(targetDir, 'doc'), dir, { recursive: true, filter: (src) => !src.endsWith('.lock') });
  // rustdoc's own pages link the documentation root, which a workspace build
  // leaves without a page; it opens the server crate.
  await fs.writeFile(
    join(dir, 'index.html'),
    `<!DOCTYPE html>\n<meta http-equiv="refresh" content="0; url=${CRATES[0].dir}/index.html">\n<a href="${CRATES[0].dir}/index.html">${CRATES[0].pkg}</a>\n`,
  );
}

if (archiveDir) {
  cargoDoc();
  await copyRustdoc(archiveDir);
  console.log(`generate-rust-docs: rustdoc of ${CRATES.map((c) => c.pkg).join(', ')} written to ${archiveDir}`);
  process.exit(0);
}

try {
  cargoDoc();
} catch (err) {
  // A site contributor without the toolchain or the native libraries can
  // still build the rest of the site against a previously generated
  // reference; a from-scratch build has nothing to fall back on.
  if (existsSync(join(rustdocDir, 'selkies', 'index.html')) && existsSync(join(outDir, 'index.md'))) {
    console.warn(`generate-rust-docs: cargo doc failed (${err.message}); reusing the existing public/rustdoc`);
    process.exit(0);
  }
  console.error(
    `generate-rust-docs: cargo doc failed and no previous public/rustdoc exists.\n${err.message}\n` +
      'The Rust toolchain and the native libraries pixelflux and pcmflux build against are required to build the Developer Reference.',
  );
  process.exit(1);
}

await copyRustdoc(rustdocDir);

const crateLinks = CRATES.map((c) => `- [\`${c.pkg}\`](/rustdoc/${c.dir}/index.html): ${c.summary}`).join('\n');
const index = `---
title: Developer Reference
description: The server's crates and the web client, generated from the docblocks in the code.
---

The server is the Rust workspace under \`crates/\`, and its reference is rustdoc's, private items included:

${crateLinks}

The web client core and the dashboard follow in the sidebar.
`;

await fs.rm(outDir, { recursive: true, force: true });
await fs.mkdir(outDir, { recursive: true });
await fs.writeFile(join(outDir, 'index.md'), index);

// The folder's sidebar entry: the index page, then a link into each crate's
// rustdoc; generate-web-docs.mjs appends the web sections.
await fs.writeFile(
  join(outDir, 'meta.json'),
  JSON.stringify(
    {
      title: 'Developer Reference',
      pages: ['index', '---Server---', ...CRATES.map((c) => `[${c.pkg}](/rustdoc/${c.dir}/index.html)`)],
    },
    null,
    2,
  ) + '\n',
);

console.log(`generate-rust-docs: rustdoc of ${CRATES.map((c) => c.pkg).join(', ')} copied to ${rustdocDir}`);
