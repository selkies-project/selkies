/**
 * What build-versions.mjs and build-reference-archive.mjs share: running a
 * command, exporting a release tag into a tree that this tree's site tooling
 * builds, and the name and address of a release's Developer Reference
 * download.
 *
 * A release's reference is a file attached to its GitHub release rather than
 * pages of the site, so the site's page for it and the release workflow that
 * attaches it have to agree on one name: `referenceAsset` is that name.
 */
import { spawnSync } from 'node:child_process';
import { cp, mkdir, rm } from 'node:fs/promises';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

/** The site directory this tooling lives in. */
export const site = dirname(dirname(fileURLToPath(import.meta.url)));
/** The repository the site directory belongs to. */
export const repoRoot = dirname(site);
/** The site directory's name, relative to the repository root. */
export const siteDir = basename(site);

/** The GitHub repository releases are attached to. */
export const repository = process.env.GITHUB_REPOSITORY || 'selkies-project/selkies';

/**
 * Runs `cmd` with its output on this process's own.
 *
 * @throws when the command does not exit with status 0.
 */
export function run(cmd, args, opts = {}) {
  const res = spawnSync(cmd, args, { stdio: 'inherit', ...opts });
  if (res.status !== 0) {
    throw new Error(`${cmd} ${args.join(' ')} exited with ${res.status ?? res.error}`);
  }
}

/**
 * Runs git in the repository and returns what it printed.
 *
 * @throws when git does not exit with status 0.
 */
export function git(args) {
  const res = spawnSync('git', args, { cwd: repoRoot, encoding: 'utf8' });
  if (res.status !== 0) throw new Error(`git ${args.join(' ')} exited with ${res.status}: ${res.stderr}`);
  return res.stdout;
}

/** The file name a release's Developer Reference is attached under, beside its packages. */
export function referenceAsset(version) {
  return `selkies-${version}-reference.tar.gz`;
}

/** The address the release tagged `tag` serves its Developer Reference download from. */
export function referenceAssetUrl(tag, version) {
  return `https://github.com/${repository}/releases/download/${encodeURIComponent(tag)}/${referenceAsset(version)}`;
}

/**
 * Exports `tag` under `tree` and gives it this tree's site tooling.
 *
 * The tag's copy of the site directory gives way to the tracked and unignored
 * files of the working tree, so a local change to the tooling is what every
 * version is built with, and nothing generated or installed leaks in.
 * node_modules is copied rather than linked: Turbopack refuses a node_modules
 * that resolves outside the tree it builds.
 */
export async function exportTag(tag, tree) {
  console.log(`version-tree: exporting ${tag}`);
  await mkdir(tree, { recursive: true });
  const archive = `${tree}.tar`;
  git(['archive', '--format=tar', `--output=${archive}`, tag]);
  run('tar', ['-xf', archive, '-C', tree]);
  await rm(archive);

  await rm(join(tree, siteDir), { recursive: true });
  const tooling = git(['ls-files', '-z', '--cached', '--others', '--exclude-standard', '--', siteDir])
    .split('\0')
    .filter(Boolean);
  for (const file of tooling) {
    await mkdir(join(tree, dirname(file)), { recursive: true });
    await cp(join(repoRoot, file), join(tree, file));
  }
  run('cp', ['-a', join(site, 'node_modules'), join(tree, siteDir, 'node_modules')]);
}
