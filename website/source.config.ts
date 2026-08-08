import { nodeTypes } from '@mdx-js/mdx';
import { defineConfig } from 'fumadocs-mdx/config';
import rehypeRaw from 'rehype-raw';
import { remarkDocsCompat } from './lib/remark-docs-compat';

export default defineConfig({
  mdxOptions: {
    remarkPlugins: (plugins) => [remarkDocsCompat, ...plugins],
    // Pages are plain Markdown, where inline HTML is a string rather than a
    // component. Without this the `<details>` blocks the FAQ is built from,
    // and everything inside them, are dropped from the page.
    remarkRehypeOptions: { allowDangerousHtml: true },
    rehypePlugins: (plugins) => [[rehypeRaw, { passThrough: nodeTypes }], ...plugins],
  },
});
