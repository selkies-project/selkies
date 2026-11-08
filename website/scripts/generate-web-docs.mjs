/**
 * Builds the web halves of the Developer Reference under docs/reference from
 * the JSDoc blocks and type annotations in the web packages.
 *
 * Runs after the server's half (generate-rust-docs.mjs, or
 * generate-python-docs.mjs for a release whose server is Python) from
 * generate-reference.mjs and adds one gitignored folder per package
 * beside it. Both packages are TypeScript, so TypeDoc reads them through one
 * configuration (`allowJs` stays on for the build helpers and any JavaScript
 * a tree still carries); typedoc-plugin-markdown writes the pages
 * as Markdown (not MDX, so nothing in a comment can be mistaken for JSX) and
 * web-reference-plugin.mjs adds front matter and hides `_`-prefixed members.
 *
 * Third-party types the packages import (Base UI, lucide, ...) are not installed
 * here and render as `any`; React's are, so component signatures resolve.
 * Type errors never fail the build: the packages' own lint and typecheck steps
 * own correctness, this script only documents.
 *
 * `--archive <dir> --ref <ref>` writes each package with TypeDoc's own HTML
 * theme into `<dir>/<package>` instead, for the download
 * build-reference-archive.mjs attaches to a release, its "Defined in" links
 * naming the source at `<ref>`. That theme links its pages relatively and
 * loads its search and navigation by script tag, so it opens from a file
 * with both working; the Markdown the site renders would open as plain text.
 */
import * as fs from 'node:fs/promises';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Minimatch } from 'minimatch';
import { Application, TSConfigReader } from 'typedoc';

// TypeDoc 0.28 walks its entry points with Minimatch#matchOne, which minimatch
// 11 dropped; 11.0.1 is the first release without GHSA-g3hv-75wg-m249, so the
// website overrides TypeDoc's minimatch to it. TypeDoc asks whether any row of
// the pattern partially matches a directory's path segments, which a partial
// match of the joined path answers, as its own file test joins them.
if (typeof Minimatch.prototype.matchOne !== 'function') {
  Minimatch.prototype.matchOne = function matchOne(file, _pattern, partial) {
    return this.match(Array.isArray(file) ? file.join('/') : file, partial);
  };
}

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const repoRoot = join(root, '..');
// A release from before the web client moved out of addons/ keeps its packages there.
const web = join(repoRoot, (await fs.stat(join(repoRoot, 'web')).catch(() => null)) ? 'web' : 'addons');
const referenceDir = join(repoRoot, 'docs', 'reference');

// Generated pages that make no sense in a reference: the translation table
// and the dashboard's vendored shadcn/ui primitives.
const EXCLUDE = [
  '**/node_modules/**',
  '**/translations*.{js,ts}',
  '**/selkies-dashboard/src/components/ui/**',
];

// `source` is the directory module names and "Defined in" paths are relative to.
const PACKAGES = [
  {
    dir: 'web-core',
    title: 'Web client core',
    source: 'selkies-web-core',
    entryPoints: ['selkies-core.ts', 'selkies-ws-core.ts', 'selkies-wr-core.ts', 'clipboard-worker.ts', 'lib'],
  },
  { dir: 'dashboard', title: 'Dashboard', source: 'selkies-dashboard/src', entryPoints: ['.'] },
];

const archiveAt = process.argv.indexOf('--archive');
const archiveDir = archiveAt === -1 ? undefined : resolve(process.argv[archiveAt + 1] ?? '');
const refAt = process.argv.indexOf('--ref');
const sourceRef = refAt === -1 ? 'main' : process.argv[refAt + 1];

// typedoc-plugin-markdown's options, for the pages the site renders.
const MARKDOWN = {
  outputFileStrategy: 'modules',
  fileExtension: '.md',
  entryFileName: 'index.md',
  hidePageHeader: true,
  hideBreadcrumbs: true,
  hidePageTitle: true,
  useCodeBlocks: true,
  parametersFormat: 'table',
  sanitizeComments: true,
};

/** Renders one package into `out`, as the site's Markdown or, for `html`, TypeDoc's own theme. */
async function generate({ title, source, entryPoints }, out, html) {
  const plugins = [join(root, 'scripts', 'web-reference-plugin.mjs')];
  const app = await Application.bootstrapWithPlugins(
    {
      name: title,
      entryPoints: entryPoints.map((p) => join(web, source, p)),
      entryPointStrategy: 'expand',
      exclude: EXCLUDE,
      tsconfig: join(root, 'scripts', 'web-reference.tsconfig.json'),
      plugin: html ? plugins : ['typedoc-plugin-markdown', ...plugins],
      out,
      readme: 'none',
      skipErrorChecking: true,
      excludeExternals: true,
      excludePrivate: true,
      disableGit: true,
      basePath: join(web, source),
      sourceLinkTemplate: `https://github.com/selkies-project/selkies/blob/${sourceRef}/${relative(repoRoot, web)}/${source}/{path}#L{line}`,
      logLevel: 'Warn',
      sort: ['source-order'],
      ...(html ? { githubPages: false } : MARKDOWN),
    },
    [new TSConfigReader()],
  );
  const project = await app.convert();
  if (!project) throw new Error(`generate-web-docs: TypeDoc could not convert ${title}`);
  await fs.rm(out, { recursive: true, force: true });
  await app.generateOutputs(project);
  const extension = html ? '.html' : '.md';
  const pages = (await fs.readdir(out, { recursive: true })).filter((f) => f.endsWith(extension)).length;
  console.log(`generate-web-docs: wrote ${pages} pages to ${html ? out : relative(repoRoot, out)}`);
}

if (archiveDir) {
  for (const pkg of PACKAGES) await generate(pkg, join(archiveDir, pkg.dir), true);
  process.exit(0);
}

await fs.mkdir(referenceDir, { recursive: true });
for (const pkg of PACKAGES) {
  const out = join(referenceDir, pkg.dir);
  await generate(pkg, out, false);
  // Sidebar order follows the source tree; `...` picks up the nested folders
  // (lib/, components/) that TypeDoc mirrors from the package.
  await fs.writeFile(
    join(out, 'meta.json'),
    JSON.stringify({ title: pkg.title, pages: ['index', '...'] }, null, 2) + '\n',
  );
}

// The folder's meta.json is written by the server's half; the web sections
// follow it behind a separator.
const metaPath = join(referenceDir, 'meta.json');
let meta = { title: 'Developer Reference', pages: ['index'] };
try {
  meta = JSON.parse(await fs.readFile(metaPath, 'utf8'));
} catch {}
const webPages = ['---Web client---', ...PACKAGES.map((a) => a.dir)];
meta.pages = [...meta.pages.filter((p) => !webPages.includes(p)), ...webPages];
await fs.writeFile(metaPath, JSON.stringify(meta, null, 2) + '\n');
