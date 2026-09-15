/**
 * Builds the versioned site into out/: one static export per release tag that
 * carries the site and one for the branch this tree is on. The release GitHub
 * designates as the latest (the newest tag where that cannot be asked) is built
 * under the `latest` alias, which the site root redirects to, and its own
 * segment redirects there page by page rather than carrying a second copy of
 * the largest build.
 *
 * Every version is rendered by this tree's site tooling over that version's
 * own pages and source, so a fix to the site reaches every version the next
 * time the site is published. A tag is exported with `git archive` into a
 * temporary tree that gets this tree's site directory and node_modules; the
 * branch builds in place.
 *
 * Each build receives NEXT_PUBLIC_BASE_PATH=<site path>/<segment> and the
 * version index as NEXT_PUBLIC_DOCS_VERSIONS, which lib/shared.ts reads. The
 * index is also written to out/versions.json. NEXT_PUBLIC_BASE_PATH on entry
 * is the path the whole site is served from, empty at a domain root.
 */
import { spawnSync } from 'node:child_process';
import { cp, mkdir, mkdtemp, readdir, rename, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const site = dirname(dirname(fileURLToPath(import.meta.url)));
const repoRoot = dirname(site);
const siteDir = basename(site);
// The branch the working tree stands for; gitConfig.branch in lib/shared.ts.
const BRANCH = 'main';
const LATEST = 'latest';
const sitePath = process.env.NEXT_PUBLIC_BASE_PATH ?? '';

function run(cmd, args, opts = {}) {
  const res = spawnSync(cmd, args, { stdio: 'inherit', ...opts });
  if (res.status !== 0) {
    throw new Error(`${cmd} ${args.join(' ')} exited with ${res.status ?? res.error}`);
  }
}

function git(args) {
  const res = spawnSync('git', args, { cwd: repoRoot, encoding: 'utf8' });
  if (res.status !== 0) throw new Error(`git ${args.join(' ')} exited with ${res.status}: ${res.stderr}`);
  return res.stdout;
}

// A version tag: a dotted number with an optional pre-release or post-release
// suffix in PEP 440 or SemVer spelling (2.0.0rc0, 2.0.0-beta.1, 2.0.0.post1).
// The dot is required, so a tag named after a commit hash, as automated
// pre-releases are, is never read as a version: 157a861 is not 157 alpha 861.
const VERSION = /^v?(\d+(?:\.\d+)+)(?:[-.]?(dev|a|alpha|b|beta|rc|pre|post)[-.]?(\d*))?$/i;
const RANK = { dev: 0, a: 1, alpha: 1, b: 2, beta: 2, rc: 3, pre: 3, final: 4, post: 5 };

function parseVersion(tag) {
  const match = VERSION.exec(tag);
  if (!match) return undefined;
  const [, numbers, label = 'final', count] = match;
  return { numbers: numbers.split('.').map(Number), rank: RANK[label.toLowerCase()], count: Number(count || 0) };
}

function compareVersions(a, b) {
  for (let i = 0; i < Math.max(a.numbers.length, b.numbers.length); i++) {
    const delta = (a.numbers[i] ?? 0) - (b.numbers[i] ?? 0);
    if (delta) return delta;
  }
  return a.rank - b.rank || a.count - b.count;
}

/**
 * The release tags whose tree carries the site, oldest first. A version tagged
 * both with and without the `v` prefix is one release, built from the bare tag.
 */
function releases() {
  const found = new Map();
  for (const tag of git(['tag', '--list']).split('\n').filter(Boolean)) {
    const parsed = parseVersion(tag);
    if (!parsed) continue;
    const segment = tag.replace(/^v/, '');
    if (found.has(segment) && tag !== segment) continue;
    const probe = spawnSync('git', ['cat-file', '-e', `${tag}:${siteDir}/package.json`], { cwd: repoRoot });
    if (probe.status === 0) found.set(segment, { tag, segment, parsed });
  }
  return [...found.values()].sort((a, b) => compareVersions(a.parsed, b.parsed));
}

/** Exports `tag` under `tree` and gives it this tree's site tooling. */
async function exportTag(tag, tree) {
  console.log(`build-versions: exporting ${tag}`);
  await mkdir(tree, { recursive: true });
  const archive = `${tree}.tar`;
  git(['archive', '--format=tar', `--output=${archive}`, tag]);
  run('tar', ['-xf', archive, '-C', tree]);
  await rm(archive);

  // The site directory is the tooling's, and the tag's copy of it gives way to
  // the tracked and unignored files of the working tree, so a local change to
  // the tooling is what every version is built with, and nothing generated or
  // installed leaks in.
  await rm(join(tree, siteDir), { recursive: true });
  const tooling = git(['ls-files', '-z', '--cached', '--others', '--exclude-standard', '--', siteDir])
    .split('\0')
    .filter(Boolean);
  for (const file of tooling) {
    await mkdir(join(tree, dirname(file)), { recursive: true });
    await cp(join(repoRoot, file), join(tree, file));
  }
  // Copied rather than linked: Turbopack refuses a node_modules that resolves
  // outside the tree it builds.
  run('cp', ['-a', join(site, 'node_modules'), join(tree, siteDir, 'node_modules')]);
}

/** Moves a directory, copying when the destination is on another filesystem. */
async function move(from, to) {
  await mkdir(dirname(to), { recursive: true });
  try {
    await rename(from, to);
  } catch (err) {
    if (err.code !== 'EXDEV') throw err;
    await cp(from, to, { recursive: true });
    await rm(from, { recursive: true });
  }
}

/** Builds the site directory under `tree` as `segment`, into `staging`. */
async function build(tree, segment, index, staging) {
  console.log(`build-versions: building ${segment}`);
  const cwd = join(tree, siteDir);
  run('npm', ['run', 'build'], {
    cwd,
    env: {
      ...process.env,
      NEXT_PUBLIC_BASE_PATH: `${sitePath}/${segment}`,
      NEXT_PUBLIC_DOCS_VERSIONS: JSON.stringify(index),
    },
  });
  await move(join(cwd, 'out'), join(staging, segment));
}

async function walkHtml(dir, prefix = '') {
  const found = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = `${prefix}${entry.name}`;
    if (entry.isDirectory()) found.push(...(await walkHtml(join(dir, entry.name), `${path}/`)));
    else if (entry.name.endsWith('.html')) found.push(path);
  }
  return found;
}

function redirect(to) {
  const target = JSON.stringify(to);
  return `<!DOCTYPE html>
<html lang="en">
<meta charset="utf-8">
<meta name="robots" content="noindex">
<meta http-equiv="refresh" content="0; url=${to}">
<link rel="canonical" href="${to}">
<title>Redirecting to ${to}</title>
<script>location.replace(${target} + location.hash)</script>
<a href="${to}">${to}</a>
</html>
`;
}

/**
 * Writes a redirect under `dir` for every page of the version served under
 * `target`, so an address under `dir` resolves to the same page there, hash
 * included. Next's own `_`-prefixed exports are not pages.
 */
async function writeRedirects(staging, dir, target) {
  let written = 0;
  for (const file of await walkHtml(join(staging, target))) {
    if (file.startsWith('_') || file === '404.html' || file.startsWith('404/')) continue;
    const page = `/${file.replace(/(^|\/)index\.html$|\.html$/, '')}`;
    await mkdir(join(staging, dir, dirname(file)), { recursive: true });
    await writeFile(join(staging, dir, file), redirect(`${sitePath}/${target}${page}`));
    written += 1;
  }
  console.log(`build-versions: ${written} redirects into ${target} written under ${dir ? `/${dir}` : 'the site root'}`);
}

/**
 * Writes the site root: the version index, the default version's 404 page,
 * and a redirect into the default version for the root and for every page of
 * it, so an address from before the site was versioned still resolves.
 */
async function writeRoot(staging, index) {
  await writeFile(join(staging, 'versions.json'), `${JSON.stringify(index, null, 2)}\n`);
  await writeFile(join(staging, '.nojekyll'), '');
  await cp(join(staging, index.default, '404.html'), join(staging, '404.html'));
  await writeRedirects(staging, '', index.default);
}

/** The release GitHub designates as the latest, as a version segment, or undefined where it cannot be asked. */
async function latestRelease() {
  const repo = process.env.GITHUB_REPOSITORY || 'selkies-project/selkies';
  const headers = process.env.GITHUB_TOKEN ? { authorization: `Bearer ${process.env.GITHUB_TOKEN}` } : {};
  try {
    const res = await fetch(`https://api.github.com/repos/${repo}/releases/latest`, { headers });
    return res.ok ? (await res.json()).tag_name?.replace(/^v/, '') : undefined;
  } catch {
    return undefined;
  }
}

const tags = releases();
// The `latest` alias follows the release GitHub designates as the latest, which a
// pre-release is not unless a maintainer says so, the way the floating image tags
// do; the newest tag stands in where that cannot be asked or names no built version.
const designated = await latestRelease();
const newest = tags.find((t) => t.segment === designated) ?? tags.at(-1);
const index = {
  default: newest ? LATEST : BRANCH,
  versions: [
    { version: BRANCH, kind: 'branch', aliases: [] },
    ...tags.map((t) => ({ version: t.segment, kind: 'release', aliases: t === newest ? [LATEST] : [] })).reverse(),
  ],
};
console.log(`build-versions: ${index.versions.map((v) => [v.version, ...v.aliases].join('=')).join(', ')}`);

const work = await mkdtemp(join(tmpdir(), 'build-versions-'));
try {
  const staging = join(work, 'site');
  for (const { tag, segment } of tags) {
    const tree = join(work, segment);
    await exportTag(tag, tree);
    await build(tree, segment === newest.segment ? LATEST : segment, index, staging);
    await rm(tree, { recursive: true });
  }
  await build(repoRoot, BRANCH, index, staging);

  // Checked before the root redirects exist, so a link that lost its version
  // segment resolves to nothing rather than to a redirect.
  run('node', [join(site, 'scripts', 'check-links.mjs'), staging], {
    env: { ...process.env, NEXT_PUBLIC_BASE_PATH: sitePath },
  });
  await writeRoot(staging, index);
  if (newest) await writeRedirects(staging, newest.segment, LATEST);

  const out = join(site, 'out');
  await rm(out, { recursive: true, force: true });
  await move(staging, out);
  console.log(`build-versions: site written to ${relative(process.cwd(), out) || '.'}`);
} finally {
  await rm(work, { recursive: true, force: true });
}
