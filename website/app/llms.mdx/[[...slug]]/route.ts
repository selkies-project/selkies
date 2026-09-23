import { notFound } from 'next/navigation';
import { docsLlms } from '@/lib/llms';
import { source } from '@/lib/source';

export const revalidate = false;

/**
 * One page as Markdown, at the path lib/shared.ts's getPageMarkdownUrl names:
 * the page's slugs and then content.md, which the export writes as a file
 * with that extension where a rewrite from the page's own URL is not available.
 */
export async function GET(_req: Request, { params }: RouteContext<'/llms.mdx/[[...slug]]'>) {
  const { slug } = await params;
  const page = source.getPage(slug?.slice(0, -1) ?? []);
  if (!page) notFound();
  return new Response(await docsLlms.page(page), { headers: { 'Content-Type': 'text/markdown' } });
}

export function generateStaticParams() {
  return source.generateParams().map((item) => ({ ...item, slug: [...item.slug, 'content.md'] }));
}
