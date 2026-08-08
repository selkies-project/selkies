import type { MetadataRoute } from 'next';
import { pageUrl } from '@/lib/shared';
import { source } from '@/lib/source';

export const dynamic = 'force-static';

export default function sitemap(): MetadataRoute.Sitemap {
  return source.getPages().map((page) => ({ url: pageUrl(page.url) }));
}
