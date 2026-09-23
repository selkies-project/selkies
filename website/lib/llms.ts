import { llms } from 'fumadocs-core/source';
import { siteUrl, withBasePath } from './shared';
import { source } from './source';

/** The page at `url`, as this build serves it: under the version segment of a versioned site. */
const absolute = (url: string) => `${siteUrl}${withBasePath(url)}`;

/**
 * The pages as Markdown for a language model: llms.txt indexes them, the full
 * file concatenates them, and each page is served on its own beside the copy
 * and "open in" actions of its header. Kept apart from lib/source.ts, whose
 * `defineDocs` macro the MDX loader evaluates by itself at build time.
 */
export const docsLlms = llms(source, {
  renderPage: async (page) => `# ${page.data.title} (${absolute(page.url)})

${await page.data.getText('processed')}`,
});

/**
 * The index with its links made absolute. The helper writes the page tree's
 * site-relative URLs, which a reader of a versioned build would resolve to
 * the site root rather than to the build the file came from.
 */
export const absoluteLinks = (markdown: string) => markdown.replace(/\]\(\//g, `](${absolute('/')}`);
