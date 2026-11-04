/**
 * Renders the Python reference fumadocs-python writes as static HTML that
 * opens from a file, for the download build-reference-archive.mjs attaches to
 * a release whose server is the Python package.
 *
 * fumadocs-python writes one MDX page per module and class, its members as
 * `<PyFunction>`, `<PyAttribute>`, and the like, which the site renders with
 * fumadocs-ui. That needs the whole Next.js build and its absolute `/_next`
 * paths, which do not open from a file; the processed Markdown the site also
 * serves keeps those components as JSX text. Here each page is compiled with
 * MDX's own compiler and rendered by React to plain HTML, every component
 * mapped to the element it stands for and every link into the reference
 * rewritten relative to the page, so the pages link each other on disk. The
 * MDX is generated from the package's own docstrings, the same input the
 * site compiles, so evaluating it runs nothing that is not already compiled
 * there.
 */
import * as fs from 'node:fs/promises';
import { dirname, join, posix } from 'node:path';
import { evaluate } from '@mdx-js/mdx';
import { createElement as h, Fragment } from 'react';
import * as runtime from 'react/jsx-runtime';
import { renderToStaticMarkup } from 'react-dom/server';

const STYLE = `
:root { color-scheme: light dark; --fg: #1f2328; --muted: #59636e; --bg: #fff; --line: #d1d9e0; --code: #f6f8fa; --link: #0969da; }
@media (prefers-color-scheme: dark) { :root { --fg: #e6edf3; --muted: #9198a1; --bg: #0d1117; --line: #3d444d; --code: #151b23; --link: #4493f8; } }
body { margin: 0; background: var(--bg); color: var(--fg); font: 16px/1.55 system-ui, sans-serif; }
main, nav { max-width: 60rem; margin: 0 auto; padding: 0 16px; }
nav { padding-top: 1rem; color: var(--muted); }
a { color: var(--link); }
code, pre { font-family: ui-monospace, monospace; font-size: 0.9em; }
pre { background: var(--code); padding: 0.75rem; overflow-x: auto; border-radius: 6px; }
:not(pre) > code { background: var(--code); padding: 0.1em 0.3em; border-radius: 4px; overflow-wrap: anywhere; }
.member { border-top: 1px solid var(--line); padding-top: 0.5rem; margin-top: 1rem; }
.member > h3 { margin: 0.25rem 0; font-size: 1rem; }
.kind { color: var(--muted); font-weight: normal; }
.params { margin: 0.5rem 0; padding-left: 1.25rem; }
details summary { cursor: pointer; color: var(--muted); }
`;

/** The page an address into the reference opens, relative to `from`, or the address unchanged. */
function linkFrom(from, href, pages, baseUrl) {
  if (href !== baseUrl && !href?.startsWith(`${baseUrl}/`) && !href?.startsWith(`${baseUrl}#`)) return href;
  const [path, hash] = href.split('#');
  const slug = path.slice(baseUrl.length).replace(/^\/|\/$/g, '');
  const target = [slug ? `${slug}.html` : 'index.html', `${slug}/index.html`.replace(/^\//, '')].find((p) =>
    pages.has(p),
  );
  if (!target) throw new Error(`python-reference-html: ${from} links ${href}, which is no page`);
  const relative = posix.relative(posix.dirname(from), target) || posix.basename(target);
  return hash ? `${relative}#${hash}` : relative;
}

/** The HTML each fumadocs-python component stands for, on the page at `from`. */
function components(from, pages, baseUrl) {
  const signature = (name, type, value, kind) =>
    h('code', null, kind ? h('span', { className: 'kind' }, `${kind} `) : null, name, type ?? '', value ? ` = ${value}` : '');
  return {
    a: ({ href, ...props }) => h('a', { ...props, href: linkFrom(from, href, pages, baseUrl) }),
    PyAttributes: ({ children }) => h(Fragment, null, children),
    PyAttribute: ({ name, type, value, children }) =>
      h('section', { className: 'member', id: name }, h('h3', null, signature(name, type ? `: ${type}` : '', value)), children),
    PyFunction: ({ name, type, kind, children }) =>
      h('section', { className: 'member', id: name }, h('h3', null, signature(name, type, undefined, kind)), children),
    PyParameter: ({ name, type, value, children }) =>
      h('ul', { className: 'params' }, h('li', null, signature(name, type ? `: ${type}` : '', value), children)),
    PyFunctionReturn: ({ type, children }) =>
      h('p', null, 'Returns ', type ? h('code', null, type) : null, children ? ': ' : null, children),
    PySourceCode: ({ children }) => h('details', null, h('summary', null, 'Source'), children),
    Tabs: ({ children }) => h(Fragment, null, children),
    Tab: ({ value, children }) => h('section', null, h('h2', null, value), children),
    Cards: ({ children }) => h('ul', null, children),
    Card: ({ title, href, description, children }) =>
      h('li', null, h('a', { href: linkFrom(from, href, pages, baseUrl) }, title), description ? ` ${description}` : null, children),
  };
}

/** The page at `path` of the reference, from fumadocs-python's MDX for it. */
async function render({ title, content }, html, pages, heading, baseUrl) {
  const { default: Content } = await evaluate(content, { ...runtime, development: false });
  const body = renderToStaticMarkup(h(Content, { components: components(html, pages, baseUrl) }));
  const home = posix.relative(posix.dirname(html), 'index.html') || 'index.html';
  const top = posix.relative(posix.dirname(html), '../index.html');
  const escape = (text) => text.replace(/&/g, '&amp;').replace(/</g, '&lt;');
  return `<!DOCTYPE html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escape(title)} · ${escape(heading)}</title>
<style>${STYLE}</style>
<nav><a href="${top}">Developer Reference</a> / <a href="${home}">${escape(heading)}</a></nav>
<main>
<h1>${escape(title)}</h1>
${body}
</main>
</html>
`;
}

/**
 * Writes `files`, fumadocs-python's pages under `baseUrl`, as HTML into
 * `outDir`, each `<module>.mdx` as `<module>.html` and each folder's index as
 * its `index.html`, `heading` naming the whole.
 *
 * @throws when a page fails to compile or links a page that is not among them.
 */
export async function writeHtml(files, outDir, heading, baseUrl) {
  const html = (path) => path.replace(/\.mdx$/, '.html');
  const pages = new Set(files.map((f) => html(f.path)));
  await fs.rm(outDir, { recursive: true, force: true });
  for (const file of files) {
    const target = join(outDir, html(file.path));
    await fs.mkdir(dirname(target), { recursive: true });
    await fs.writeFile(target, await render(file, html(file.path), pages, heading, baseUrl));
  }
  return pages.size;
}
