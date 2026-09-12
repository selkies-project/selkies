'use client';

import { Popover, PopoverContent, PopoverTrigger } from 'fumadocs-ui/components/ui/popover';
import { Check, ChevronsUpDown } from 'lucide-react';
import { usePathname } from 'next/navigation';
import { type MouseEvent, useState } from 'react';
import { currentVersion, type DocsVersion, docsVersions, servedSegment, versionPath } from '@/lib/shared';

function describe({ kind, aliases }: DocsVersion): string {
  if (aliases.includes('latest')) return 'Latest release';
  return kind === 'branch' ? 'Development branch' : 'Release';
}

function Aliases({ aliases }: Pick<DocsVersion, 'aliases'>) {
  return aliases.map((alias) => (
    <span
      key={alias}
      className="ms-1.5 rounded-md bg-fd-primary/10 px-1.5 py-0.5 text-xs font-medium text-fd-primary"
    >
      {alias}
    </span>
  ));
}

/**
 * The sidebar's version dropdown: every version of the site with the current
 * one marked, each opening the reader's page in that version.
 *
 * Versions are separate builds, so a choice leaves this one with a full
 * navigation rather than the router. A page need not exist in another
 * version: a link points at that version's index, and a click probes the
 * page's own address there first and takes it when it answers.
 */
export function VersionSwitcher() {
  const [open, setOpen] = useState(false);
  const pathname = usePathname();
  if (!docsVersions || !currentVersion) return null;

  const onClick = async (event: MouseEvent<HTMLAnchorElement>, version: DocsVersion) => {
    // A modified click opens the index in a new tab; the browser handles it.
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    setOpen(false);
    let target = event.currentTarget.href;
    const samePage = versionPath(servedSegment(version), pathname);
    try {
      if ((await fetch(samePage, { method: 'HEAD' })).ok) target = samePage + window.location.hash;
    } catch {}
    window.location.assign(target);
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger className="flex items-center gap-2 rounded-lg border bg-fd-secondary/50 p-2 text-start text-fd-secondary-foreground transition-colors hover:bg-fd-accent data-open:bg-fd-accent data-open:text-fd-accent-foreground">
        <div>
          <p className="text-sm font-medium">
            <span className="sr-only">Version </span>
            {currentVersion.version}
            <Aliases aliases={currentVersion.aliases} />
          </p>
          <p className="text-sm text-fd-muted-foreground md:hidden">{describe(currentVersion)}</p>
        </div>
        <ChevronsUpDown className="ms-auto size-4 shrink-0 text-fd-muted-foreground" />
      </PopoverTrigger>
      <PopoverContent className="fd-scroll-container flex w-(--anchor-width) flex-col gap-1 p-1">
        {docsVersions.versions.map((version) => (
          <a
            key={version.version}
            href={versionPath(servedSegment(version))}
            onClick={(event) => onClick(event, version)}
            className="flex items-center gap-2 rounded-lg p-1.5 hover:bg-fd-accent hover:text-fd-accent-foreground"
          >
            <div>
              <p className="text-sm font-medium leading-none">
                {version.version}
                <Aliases aliases={version.aliases} />
              </p>
              <p className="mt-1 text-[0.8125rem] text-fd-muted-foreground">{describe(version)}</p>
            </div>
            <Check
              className={`ms-auto size-3.5 shrink-0 text-fd-primary${version === currentVersion ? '' : ' invisible'}`}
            />
          </a>
        ))}
      </PopoverContent>
    </Popover>
  );
}
