import { loader } from 'fumadocs-core/source';
import { getSlugs } from 'fumadocs-core/source/plugins/slugs';
import { metaSchema, pageSchema } from 'fumadocs-core/source/schema';
import { defineDocs } from 'fumadocs-mdx/macro';

const docs = defineDocs({
  // Pages stay in the repository's docs/ directory rather than moving under
  // the site, so editing one through GitHub needs no knowledge of Next.js.
  dir: '../docs',
  docs: { schema: pageSchema },
  meta: { schema: metaSchema },
});

export const source = loader({
  // Pages sit at the site root rather than under a /docs prefix.
  baseUrl: '/',
  source: docs.toFumadocsSource(),
  slugs(file) {
    const segments = getSlugs(file.path);
    // docs/README.md is the landing page. Keeping that name is what makes
    // GitHub render it when someone browses the docs directory.
    if (segments.at(-1) === 'README') segments.pop();
    return segments;
  },
});
