import type { Root } from 'mdast';
import type { Transformer } from 'unified';
import { visit } from 'unist-util-visit';

const PROTOCOL = /^[a-z][a-z0-9+.-]*:/i;

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
 *   are pointed at it.
 * - The coTURN configuration blocks are tagged `conf`, the name Pygments gives
 *   that grammar. Shiki calls it `ini`.
 */
export function remarkDocsCompat(): Transformer<Root, Root> {
  return (tree) => {
    visit(tree, 'link', (node) => {
      if (PROTOCOL.test(node.url) || node.url.startsWith('/')) return;
      if (node.url.startsWith('./') || node.url.startsWith('../')) return;
      if (/\.mdx?($|#)/.test(node.url)) node.url = `./${node.url}`;
    });
    visit(tree, 'image', (node) => {
      if (node.url.startsWith('assets/')) node.url = `/${node.url}`;
    });
    visit(tree, 'code', (node) => {
      if (node.lang === 'conf') node.lang = 'ini';
    });
  };
}
