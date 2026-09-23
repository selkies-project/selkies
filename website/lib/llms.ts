import { llms } from 'fumadocs-core/source';
import { pageUrl } from './shared';
import { source } from './source';

/**
 * The pages as Markdown for a language model: llms.txt indexes them, the full
 * file concatenates them, and each page is served on its own beside the copy
 * and "open in" actions of its header. Kept apart from lib/source.ts, whose
 * `defineDocs` macro the MDX loader evaluates by itself at build time.
 */
export const docsLlms = llms(source, {
  renderPage: async (page) => `# ${page.data.title} (${pageUrl(page.url)})

${await page.data.getText('processed')}`,
});
