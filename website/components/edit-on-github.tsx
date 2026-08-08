import { gitConfig } from '@/lib/shared';

/**
 * Opens the page's Markdown source in GitHub's web editor.
 *
 * Contributing a fix should not require a checkout or a Node.js toolchain, so
 * every page carries a link straight to the file it was built from.
 */
export function EditOnGitHub({ path }: { path: string }) {
  const { user, repo, branch, dir } = gitConfig;
  const href = `https://github.com/${user}/${repo}/edit/${branch}/${dir}/${path}`;

  return (
    <a
      href={href}
      rel="noreferrer noopener"
      target="_blank"
      className="text-sm text-fd-muted-foreground hover:text-fd-accent-foreground w-fit not-prose"
    >
      Edit this page on GitHub
    </a>
  );
}
