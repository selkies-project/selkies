import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import { VersionSwitcher } from '@/components/version-switcher';
import { baseOptions } from '@/lib/layout.shared';
import { docsVersions } from '@/lib/shared';
import { source } from '@/lib/source';

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <DocsLayout
      tree={source.getPageTree()}
      // The sidebar's foot stacks the GitHub link and theme switch row above
      // its footer; ordering the dropdown first puts it above that row. A
      // plain build is the only version of itself and has nothing to switch to.
      sidebar={{ footer: docsVersions && <VersionSwitcher className="order-first mb-2" /> }}
      {...baseOptions()}
    >
      {children}
    </DocsLayout>
  );
}
