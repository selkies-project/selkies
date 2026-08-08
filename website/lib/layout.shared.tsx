import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';
import Image from 'next/image';
// Imported rather than referenced by URL: a bare path would be emitted without
// the GitHub Pages base path and 404 in production.
import icon from '../../docs/assets/logo/icon-192x192.png';
import { repoUrl, siteName } from './shared';

export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        <>
          <Image src={icon} alt="" width={24} height={24} aria-hidden />
          <span className="font-semibold">{siteName}</span>
        </>
      ),
    },
    githubUrl: repoUrl,
  };
}
