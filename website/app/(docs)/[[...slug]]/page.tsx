import { createRelativeLink } from 'fumadocs-ui/mdx';
import {
  DocsBody,
  DocsDescription,
  DocsPage,
  DocsTitle,
} from 'fumadocs-ui/layouts/docs/page';
import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { getMDXComponents } from '@/components/mdx';
import { EditOnGitHub } from '@/components/edit-on-github';
import { pageUrl, siteName } from '@/lib/shared';
import { source } from '@/lib/source';

const isLanding = (url: string) => url === '/';

export default async function Page(props: PageProps<'/[[...slug]]'>) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const MDX = page.data.body;

  return (
    <DocsPage toc={page.data.toc} full={page.data.full}>
      {/* The landing page opens with the logo, which already says Selkies. */}
      {!isLanding(page.url) && <DocsTitle>{page.data.title}</DocsTitle>}
      <DocsDescription>{page.data.description}</DocsDescription>
      <EditOnGitHub path={page.path} />
      <DocsBody>
        <MDX
          components={getMDXComponents({
            // Lets a page keep linking to `start.md`, which is what GitHub
            // resolves when the same file is read there.
            a: createRelativeLink(source, page),
          })}
        />
      </DocsBody>
    </DocsPage>
  );
}

export function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata(
  props: PageProps<'/[[...slug]]'>,
): Promise<Metadata> {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  return {
    // The landing page's own title is the site name, which the template would
    // otherwise repeat back at it.
    title: isLanding(page.url) ? { absolute: siteName } : page.data.title,
    description: page.data.description,
    alternates: { canonical: pageUrl(page.url) },
  };
}
