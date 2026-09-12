import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import { VersionSwitcher } from '@/components/version-switcher';
import { baseOptions } from '@/lib/layout.shared';
import { docsVersions } from '@/lib/shared';
import { source } from '@/lib/source';

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <DocsLayout
      tree={source.getPageTree()}
      // A plain build is the only version of itself and has nothing to switch to.
      sidebar={{ banner: docsVersions && <VersionSwitcher /> }}
      {...baseOptions()}
    >
      {children}
    </DocsLayout>
  );
}
