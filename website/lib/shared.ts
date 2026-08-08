export const siteName = 'Selkies';

export const siteDescription =
  'Open-Source Low-Latency Accelerated Linux WebSocket and WebRTC HTML5 Remote Desktop Streaming Platform for Self-Hosting, Containers, Kubernetes, or Cloud/HPC';

export const siteUrl = 'https://selkies-project.github.io/selkies';

export const discordUrl = 'https://discord.gg/wDNGDeSW5F';

// Pages live in the repository's docs/ directory, unprefixed, so a contributor
// can keep editing them straight from GitHub.
export const gitConfig = {
  user: 'selkies-project',
  repo: 'selkies',
  branch: 'main',
  dir: 'docs',
};

export const repoUrl = `https://github.com/${gitConfig.user}/${gitConfig.repo}`;

// next.config.mjs applies this to routed URLs; anything assembled by hand here
// has to add it back.
export const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? '';

export function withBasePath(path: string): string {
  return path.startsWith('/') ? `${basePath}${path}` : path;
}

/**
 * The published address of a page. Pages resolve with or without a trailing
 * slash, so one spelling is named as canonical and it is this one.
 */
export function pageUrl(url: string): string {
  return `${siteUrl}${url === '/' ? '/' : url}`;
}
