/**
 * Builds the Developer Reference as the one file a release carries for it,
 * `selkies-<version>-reference.tar.gz`, which the site's page for the
 * reference of that version links (generate-reference.mjs).
 *
 * Usage: node website/scripts/build-reference-archive.mjs <version> [--tag <tag>] [--out <dir>]
 *
 * Without `--tag` the reference is of this tree, as a release builds it from
 * the commit it releases (build-reference.yaml); with one it is of that tag,
 * exported the way build-versions.mjs exports one and rendered by this tree's
 * tooling, which is how a release cut before its reference was a download
 * gets one.
 * The file is written to `--out`, the working directory by default. Its
 * source links name the commit it was built from, which a version stamped
 * for a build of the branch (`0.0.0.dev0`) does not.
 *
 * The site's pages load everything from absolute `/_next` paths and so open
 * only from a server. The archive holds
 * each half in a form that opens from a file instead, below one directory
 * named after the file, with an `index.html` linking them:
 *
 * - `rustdoc/`, the server's crates as rustdoc renders them, for a tree whose
 *   server is the Rust workspace (generate-rust-docs.mjs);
 * - `python/`, the server's modules as static HTML, for a tree whose server
 *   is the Python package (generate-python-docs.mjs);
 * - `web-core/` and `dashboard/`, the web client as TypeDoc's own HTML theme
 *   renders it (generate-web-docs.mjs).
 *
 * rustdoc and TypeDoc both link relatively and load their search by script
 * tag, which a browser runs from a file, so their search works offline.
 */
import { existsSync } from 'node:fs';
import { mkdtemp, mkdir, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { exportTag, git, referenceAsset, repoRoot, run, siteDir } from './version-tree.mjs';

// What GitHub accepts as one release asset.
const ASSET_LIMIT = 2 * 1024 ** 3;

function option(name) {
  const at = process.argv.indexOf(name);
  return at === -1 ? undefined : process.argv[at + 1];
}

const version = process.argv[2];
if (!version || version.startsWith('--')) {
  console.error('Usage: node build-reference-archive.mjs <version> [--tag <tag>] [--out <dir>]');
  process.exit(2);
}
const tag = option('--tag');
const outDir = resolve(option('--out') ?? '.');
const name = referenceAsset(version).replace(/\.tar\.gz$/, '');
const commit = git(['rev-parse', `${tag ?? 'HEAD'}^{commit}`]).trim();

/** The entry page of the archive, linking each half `halves` lists. */
function entryPage(halves) {
  const items = halves.map(({ href, title, summary }) => `<li><a href="${href}">${title}</a>: ${summary}</li>`).join('\n');
  return `<!DOCTYPE html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Selkies ${version} Developer Reference</title>
<style>
:root { color-scheme: light dark; }
body { margin: 0 auto; max-width: 60rem; padding: 0 16px; font: 16px/1.55 system-ui, sans-serif; }
</style>
<h1>Selkies ${version} Developer Reference</h1>
<p>Generated from the docblocks in the code of Selkies ${version}, commit <code>${commit.slice(0, 12)}</code>.</p>
<ul>
${items}
</ul>
</html>
`;
}

const work = await mkdtemp(join(tmpdir(), 'reference-archive-'));
try {
  const tree = tag ? join(work, 'tree') : repoRoot;
  if (tag) await exportTag(tag, tree);
  const scripts = join(tree, siteDir, 'scripts');
  const staging = join(work, name);
  await mkdir(staging);

  const halves = [];
  if (existsSync(join(tree, 'crates', 'selkies', 'Cargo.toml'))) {
    run('node', [join(scripts, 'generate-rust-docs.mjs'), '--archive', join(staging, 'rustdoc')]);
    halves.push(
      { href: 'rustdoc/selkies/index.html', title: '<code>selkies</code>', summary: 'the streaming server and the <code>selkies</code> executable' },
      { href: 'rustdoc/selkies_rtc/index.html', title: '<code>selkies-rtc</code>', summary: 'its WebRTC stack' },
      { href: 'rustdoc/pixelflux/index.html', title: '<code>pixelflux</code>', summary: 'screen capture and video encoding' },
      { href: 'rustdoc/pcmflux/index.html', title: '<code>pcmflux</code>', summary: 'audio capture, playback, and Opus encoding' },
    );
  }
  if (existsSync(join(tree, 'src', 'selkies', '__init__.py'))) {
    run('node', [join(scripts, 'generate-python-docs.mjs'), '--archive', join(staging, 'python')]);
    halves.push({ href: 'python/index.html', title: 'Python server', summary: 'the <code>selkies</code> package and its modules' });
  }
  if (!halves.length) throw new Error(`build-reference-archive: ${tag ?? 'this tree'} has no server to document`);
  run('node', [join(scripts, 'generate-web-docs.mjs'), '--archive', staging, '--ref', commit]);
  halves.push(
    { href: 'web-core/index.html', title: 'Web client core', summary: 'the streaming cores and their library' },
    { href: 'dashboard/index.html', title: 'Dashboard', summary: 'the dashboard the web client is driven from' },
  );
  await writeFile(join(staging, 'index.html'), entryPage(halves));

  await mkdir(outDir, { recursive: true });
  const file = join(outDir, referenceAsset(version));
  run('tar', ['-czf', file, '-C', work, name]);
  const { size } = await stat(file);
  if (size >= ASSET_LIMIT) throw new Error(`build-reference-archive: ${file} is ${size} bytes, over what a release asset may be`);
  console.log(`build-reference-archive: ${file} (${(size / 1e6).toFixed(1)} MB)`);
} finally {
  await rm(work, { recursive: true, force: true });
}
