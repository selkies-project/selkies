import { absoluteLinks, docsLlms } from '@/lib/llms';

// A static file of the export, so a GitHub Pages site serves it as it does the
// pages; the same for llms-full.txt and every page's Markdown.
export const revalidate = false;

export async function GET() {
  return new Response(absoluteLinks(await docsLlms.index()), { headers: { 'Content-Type': 'text/plain' } });
}
