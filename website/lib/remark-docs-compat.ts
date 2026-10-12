import { join, posix, relative, sep } from 'node:path';
import type { Root } from 'mdast';
import type { Transformer } from 'unified';
import { visit } from 'unist-util-visit';
import { siteUrl, withBasePath } from './shared';

const PROTOCOL = /^[a-z][a-z0-9+.-]*:/i;

// The pages' directory, beside the site's (next.config.mjs roots the bundler above both).
const docsDir = join(process.cwd(), '..', 'docs');

/**
 * Reconciles the Markdown dialect the pages are written in with this site's.
 *
 * Pages are authored for GitHub first, so nothing here pushes a change back
 * into the Markdown: whatever GitHub renders correctly is what the files keep
 * saying.
 *
 * - A sibling page is linked as `start.md`. Fumadocs resolves such a link to
 *   its page URL only once it is explicitly relative.
 * - Images are written relative to the Markdown file. Every page but the
 *   landing page is served one path segment deep, so the same relative URL
 *   would miss; sync-assets.mjs mirrors docs/assets to the site root and these
 *   are pointed at it (`remarkSiteImages` then gives them their address).
 * - The coTURN configuration blocks are tagged `conf`. Shiki knows that
 *   grammar as `ini`.
 */
export function remarkDocsCompat(): Transformer<Root, Root> {
  return (tree, file) => {
    visit(tree, 'link', (node) => {
      if (PROTOCOL.test(node.url) || node.url.startsWith('/')) return;
      if (node.url.startsWith('./') || node.url.startsWith('../')) return;
      if (/\.mdx?($|#)/.test(node.url)) node.url = `./${node.url}`;
    });
    visit(tree, 'image', (node) => {
      if (PROTOCOL.test(node.url) || node.url.startsWith('/') || !file.dirname) return;
      const path = posix.join(relative(docsDir, file.dirname).split(sep).join('/'), node.url);
      if (path.startsWith('assets/')) node.url = `/${path}`;
    });
    visit(tree, 'code', (node) => {
      if (node.lang === 'conf') node.lang = 'ini';
    });
  };
}

/**
 * Images, once remarkImage has read their size from public/: the page loads
 * each from there under the base path, and the Markdown copy of the page
 * (lib/llms.ts) names it by its full address. Imported into the bundle
 * instead, an image left only its import's name in that copy.
 */
export function remarkSiteImages(): Transformer<Root, Root> {
  return (tree) => {
    visit(tree, 'image', (node) => {
      if (!node.url.startsWith('/') || node.url.startsWith('//')) return;
      node.data ??= {};
      node.data.hProperties = { ...node.data.hProperties, src: withBasePath(node.url) };
      node.url = `${siteUrl}${withBasePath(node.url)}`;
    });
  };
}
