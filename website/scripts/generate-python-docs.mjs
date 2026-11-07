/**
 * Builds the Developer Reference pages under docs/reference from the Google-
 * style docstrings and type hints in src/selkies, for a version whose server
 * is that Python package: the releases build:versions renders with this
 * tree's tooling. A tree without it is left to generate-rust-docs.mjs.
 *
 * The pages are build output, not source: docs/reference is gitignored and
 * this script runs from the predev/prebuild npm hooks, so a local build and
 * the Pages workflow both generate them fresh from the code.
 *
 * Extraction is done by fumapy-generate (griffe, static analysis — the
 * package's native dependencies are never imported). It runs from a private
 * venv in website/.venv-docs that this script bootstraps on first use with
 * python3; delete that directory to force a clean bootstrap, e.g. after
 * upgrading the fumadocs-python npm package. Passing a JSON path as the first
 * argument skips extraction and converts that file instead.
 *
 * `--archive <dir>` writes the pages as static HTML into `<dir>` instead
 * (python-reference-html.mjs), for the download build-reference-archive.mjs
 * attaches to a release, and leaves the site's pages alone; that build has no
 * earlier reference to fall back on and fails outright.
 */
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import * as fs from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as Python from 'fumadocs-python';
import { writeHtml } from './python-reference-html.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const repoRoot = join(root, '..');
const outDir = join(repoRoot, 'docs', 'reference');
const venv = join(root, '.venv-docs');
const venvBin = join(venv, process.platform === 'win32' ? 'Scripts' : 'bin');

// Vendored forks (python-xlib, aiortc, aioice) keep upstream documentation
// style and are not part of the Selkies API surface the reference covers.
const EXCLUDED_MODULES = ['Xlib', 'webrtc', 'ice'];

function run(cmd, args, opts = {}) {
  const res = spawnSync(cmd, args, { stdio: 'inherit', ...opts });
  if (res.status !== 0) {
    throw new Error(`${cmd} ${args.join(' ')} exited with ${res.status ?? res.error}`);
  }
}

function extract() {
  const fumapy = join(venvBin, 'fumapy-generate');
  if (!existsSync(fumapy)) {
    run('python3', ['-m', 'venv', venv]);
    const pip = join(venvBin, 'pip');
    // --no-deps: griffe reads the source statically, so selkies only needs to
    // exist as installed metadata, not with its native dependencies.
    run(pip, ['install', '--quiet', '--no-deps', '-e', repoRoot]);
    run(pip, ['install', '--quiet', join(root, 'node_modules', 'fumadocs-python')]);
  }
  run(fumapy, ['selkies', '--dir', root], { cwd: join(repoRoot, 'src') });
  return join(root, 'selkies.json');
}

const args = process.argv.slice(2);
const archiveAt = args.indexOf('--archive');
const archiveDir = archiveAt === -1 ? undefined : resolve(args.splice(archiveAt, 2)[1] ?? '');
let jsonPath = args[0];
if (!jsonPath && !existsSync(join(repoRoot, 'src', 'selkies', '__init__.py'))) {
  console.log('generate-python-docs: no Python package in this tree; skipped');
  process.exit(0);
}
if (!jsonPath) {
  try {
    jsonPath = extract();
  } catch (err) {
    if (archiveDir) throw err;
    // A site contributor without a working python3 can still build the rest
    // of the site against a previously generated reference; a from-scratch
    // build has nothing to fall back on.
    if (existsSync(join(outDir, 'index.mdx'))) {
      console.warn(`generate-python-docs: extraction failed (${err.message}); reusing the existing docs/reference`);
      process.exit(0);
    }
    console.error(`generate-python-docs: extraction failed and no previous docs/reference exists.\n${err.message}\npython3 with the venv module is required to build the Developer Reference.`);
    process.exit(1);
  }
}

const pkg = JSON.parse(await fs.readFile(jsonPath, 'utf8'));
for (const name of EXCLUDED_MODULES) delete pkg.modules[name];

// groupBy 'none' keeps the pages at the root of the output directory, so a
// module is published as /reference/<module> rather than under the package.
const files = Python.convert(pkg, { baseUrl: '/reference', groupBy: 'none' });

if (archiveDir) {
  const pages = await writeHtml(files, archiveDir, 'Python server', '/reference');
  console.log(`generate-python-docs: wrote ${pages} pages to ${archiveDir}`);
  process.exit(0);
}

await fs.rm(outDir, { recursive: true, force: true });
await Python.write(files, outDir);

// The folder's sidebar entry: the package page first, then every module in
// the order the extractor saw them.
const moduleNames = Object.keys(pkg.modules);
await fs.writeFile(
  join(outDir, 'meta.json'),
  JSON.stringify({ title: 'Developer Reference', pages: ['index', ...moduleNames] }, null, 2) + '\n',
);

console.log(`generate-python-docs: wrote ${files.length} pages to ${outDir}`);
