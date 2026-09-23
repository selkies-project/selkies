import { createRelativeLink } from 'fumadocs-ui/mdx';
import {
  DocsBody,
  DocsDescription,
  DocsPage,
  DocsTitle,
  EditOnGitHub,
  MarkdownCopyButton,
  ViewOptionsPopover,
} from 'fumadocs-ui/layouts/docs/page';
import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { getMDXComponents } from '@/components/mdx';
import { gitConfig, pageMarkdownUrl, pageUrl, siteName } from '@/lib/shared';
import { source } from '@/lib/source';

const isLanding = (url: string) => url === '/';

/** The page's Markdown source, open in GitHub's web editor. */
function editUrl(path: string) {
  const { user, repo, branch, dir } = gitConfig;
  return `https://github.com/${user}/${repo}/edit/${branch}/${dir}/${path}`;
}

/** The page's Markdown source on GitHub, for the "open in" menu. */
function blobUrl(path: string) {
  const { user, repo, branch, dir } = gitConfig;
  return `https://github.com/${user}/${repo}/blob/${branch}/${dir}/${path}`;
}

export default async function Page(props: PageProps<'/[[...slug]]'>) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const MDX = page.data.body;
  const edit = <EditOnGitHub href={editUrl(page.path)} />;
  const markdownUrl = pageMarkdownUrl(page);

  return (
    <DocsPage
      toc={page.data.toc}
      full={page.data.full}
      tableOfContent={{ footer: edit }}
      tableOfContentPopover={{ footer: edit }}
    >
      {/* The landing page opens with the logo and its own lead paragraphs,
          which say all of this over again. The rule is what separates a page's
          heading from its first section, which is otherwise spaced exactly
          like one section from the next. */}
      {!isLanding(page.url) && (
        <div className="mb-2 flex flex-col gap-4 border-b border-fd-border pb-6">
          <DocsTitle>{page.data.title}</DocsTitle>
          <DocsDescription className="mb-0">{page.data.description}</DocsDescription>
        </div>
      )}
      {/* The page as Markdown: copied, or opened in a language model's chat
          with the page's URL, for the reader and for an agent alike. */}
      <div className="flex flex-row items-center gap-2 mb-4">
        <MarkdownCopyButton markdownUrl={markdownUrl} />
        <ViewOptionsPopover markdownUrl={markdownUrl} githubUrl={blobUrl(page.path)} />
      </div>
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
