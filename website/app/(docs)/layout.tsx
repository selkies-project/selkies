import { DocsLayout } from 'fumadocs-ui/layouts/spacious';
import { VersionSwitcher } from '@/components/version-switcher';
import { baseOptions } from '@/lib/layout.shared';
import { docsVersions } from '@/lib/shared';
import { source } from '@/lib/source';

export default function Layout({ children }: LayoutProps<'/'>) {
  const base = baseOptions();
  return (
    <DocsLayout
      {...base}
      tree={source.getPageTree()}
      // The Spacious sidebar has no footer: the version dropdown leads the
      // sidebar's links instead, on the desktop sidebar and the mobile drawer
      // alike. A plain build is the only version of itself and has nothing to
      // switch to.
      links={
        docsVersions
          ? [{ type: 'custom', on: 'menu', children: <VersionSwitcher className="mb-2" /> }, ...(base.links ?? [])]
          : base.links
      }
    >
      {children}
    </DocsLayout>
  );
}
