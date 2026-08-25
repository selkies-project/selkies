/**
 * Builds the web halves of the Developer Reference under docs/reference from
 * the JSDoc blocks and type annotations in the web addons.
 *
 * Runs after generate-python-docs.mjs from the same predev/prebuild npm hooks
 * and adds one gitignored folder per addon beside the Python modules. TypeDoc
 * reads plain JavaScript through the TypeScript compiler's JSDoc support and
 * TypeScript natively, so one configuration covers selkies-web-core, the
 * dashboard and the wish dashboard; typedoc-plugin-markdown writes the pages
 * as Markdown (not MDX, so nothing in a comment can be mistaken for JSX) and
 * web-reference-plugin.mjs adds front matter and hides `_`-prefixed members.
 *
 * Third-party types the addons import (Radix, Recharts, ...) are not installed
 * here and render as `any`; React's are, so component signatures resolve.
 * Type errors never fail the build: the addons' own lint and typecheck steps
 * own correctness, this script only documents.
 */
import * as fs from 'node:fs/promises';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Application, TSConfigReader } from 'typedoc';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const repoRoot = join(root, '..');
const addons = join(repoRoot, 'addons');
const referenceDir = join(repoRoot, 'docs', 'reference');

// Generated pages that make no sense in a reference: translation tables,
// the wish dashboard's vendored shadcn/ui primitives, and the built core that
// selkies-dashboard's copy-core.js drops into its src/ at build time.
const EXCLUDE = [
  '**/node_modules/**',
  '**/translations*.{js,ts}',
  '**/selkies-dashboard-wish/src/components/ui/**',
  '**/selkies-dashboard/src/selkies-core.js',
];

// `source` is the directory module names and "Defined in" paths are relative to.
const ADDONS = [
  {
    dir: 'web-core',
    title: 'Web client core',
    source: 'selkies-web-core',
    entryPoints: ['selkies-core.js', 'selkies-ws-core.js', 'selkies-wr-core.js', 'clipboard-worker.js', 'lib'],
  },
  { dir: 'dashboard', title: 'Dashboard', source: 'selkies-dashboard/src', entryPoints: ['.'] },
  { dir: 'dashboard-wish', title: 'Dashboard (Wish)', source: 'selkies-dashboard-wish/src', entryPoints: ['.'] },
];

async function generate({ dir, title, source, entryPoints }) {
  const out = join(referenceDir, dir);
  const app = await Application.bootstrapWithPlugins(
    {
      name: title,
      entryPoints: entryPoints.map((p) => join(addons, source, p)),
      entryPointStrategy: 'expand',
      exclude: EXCLUDE,
      tsconfig: join(root, 'scripts', 'web-reference.tsconfig.json'),
      plugin: ['typedoc-plugin-markdown', join(root, 'scripts', 'web-reference-plugin.mjs')],
      out,
      readme: 'none',
      skipErrorChecking: true,
      excludeExternals: true,
      excludePrivate: true,
      disableGit: true,
      basePath: join(addons, source),
      sourceLinkTemplate: `https://github.com/selkies-project/selkies/blob/main/addons/${source}/{path}#L{line}`,
      logLevel: 'Warn',
      sort: ['source-order'],
      // typedoc-plugin-markdown
      outputFileStrategy: 'modules',
      fileExtension: '.md',
      entryFileName: 'index.md',
      hidePageHeader: true,
      hideBreadcrumbs: true,
      hidePageTitle: true,
      useCodeBlocks: true,
      parametersFormat: 'table',
      sanitizeComments: true,
    },
    [new TSConfigReader()],
  );
  const project = await app.convert();
  if (!project) throw new Error(`generate-web-docs: TypeDoc could not convert ${title}`);
  await fs.rm(out, { recursive: true, force: true });
  await app.generateOutputs(project);

  // Sidebar order follows the source tree; `...` picks up the nested folders
  // (lib/, components/) that TypeDoc mirrors from the addon.
  await fs.writeFile(
    join(out, 'meta.json'),
    JSON.stringify({ title, pages: ['index', '...'] }, null, 2) + '\n',
  );
  const pages = (await fs.readdir(out, { recursive: true })).filter((f) => f.endsWith('.md')).length;
  console.log(`generate-web-docs: wrote ${pages} pages to ${relative(repoRoot, out)}`);
}

await fs.mkdir(referenceDir, { recursive: true });
for (const addon of ADDONS) await generate(addon);

// The folder's meta.json is written by generate-python-docs.mjs; the web
// sections follow the Python modules behind a separator.
const metaPath = join(referenceDir, 'meta.json');
let meta = { title: 'Developer Reference', pages: ['index'] };
try {
  meta = JSON.parse(await fs.readFile(metaPath, 'utf8'));
} catch {}
const webPages = ['---Web client---', ...ADDONS.map((a) => a.dir)];
meta.pages = [...meta.pages.filter((p) => !webPages.includes(p)), ...webPages];
await fs.writeFile(metaPath, JSON.stringify(meta, null, 2) + '\n');
