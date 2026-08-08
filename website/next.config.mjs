import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createMDX } from 'fumadocs-mdx/next';

// GitHub Pages serves this project site under /selkies, and every emitted URL
// has to carry that prefix. It comes from the environment so that a local run,
// or a fork serving from a domain root, can leave it empty.
const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? '';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  // The pages being compiled live in the repository's docs/ directory, which
  // is above this one, so the bundler has to be rooted a level up to reach it.
  turbopack: { root: join(dirname(fileURLToPath(import.meta.url)), '..') },
  // GitHub Pages serves files, not a Node.js server.
  output: 'export',
  basePath,
  // The optimizer is a server, which a static export does not have.
  images: { unoptimized: true },
  reactStrictMode: true,
};

export default withMDX(config);
