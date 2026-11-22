/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';

// The database is curated continuously, so a build takes its branch's newest
// commit rather than a pinned one, and checks the file it downloads against a
// second copy of it at that commit: the two come over separate requests to
// separate hosts and must agree, compared by SHA-256. The second copy is the
// one GitHub's API serves. Where the API refuses, as it does an address that
// has spent the 60 requests an hour it allows without a token, the commit is
// the one git's own HTTP protocol advertises for the branch and the copy the
// one in that commit's tarball, which the hourly limit does not cover.
const DB_REPO = 'mdqinc/SDL_GameControllerDB';
const DB_BRANCH = 'master';
const DB_FILE = 'gamecontrollerdb.txt';
const API = 'https://api.github.com';
const GIT = 'https://github.com';
const CODELOAD = 'https://codeload.github.com';
const OUTPUT_DIR = 'dist/jsdb';
// A fetch that fails is tried again this many times in all, this far apart.
const FETCH_ATTEMPTS = 3;
const FETCH_RETRY_MS = 5000;

const VALID_MAPPINGS = new Set([
  'a', 'b', 'x', 'y', 'back', 'guide', 'start', 'leftstick', 'rightstick',
  'leftshoulder', 'rightshoulder', 'lefttrigger', 'righttrigger',
  'dpup', 'dpdown', 'dpleft', 'dpright',
  'leftx', 'lefty', 'rightx', 'righty'
]);

// The SDL DB carries one line per platform for the same vendor-product GUID and
// the raw button/axis indices differ between them, so each mapping is written
// into a per-platform subdirectory that lib/gamepad.ts selects by user agent.
// Platforms outside this set have no browser target and are skipped.
const PLATFORM_DIRS: Record<string, string> = {
  'windows': 'windows',
  'mac os x': 'mac',
  'linux': 'linux',
  'android': 'android',
  'ios': 'ios'
};

/** One control of a pad as the browser reports it raw: an axis, a button, or a hat direction. */
interface RawControl {
  type: 'axis' | 'button' | 'hat';
  index: number;
  mask?: number;
}

/** A pad's mapping file: its path under the output directory and its controls by SDL name. */
interface PadMapping {
  filename: string;
  mapping: Record<string, RawControl>;
}

function parseSdlLine(line: string): PadMapping | null {
  if (line.startsWith('#') || line.trim() === '') {
    return null;
  }

  const parts = line.split(',');
  const guid = parts[0];

  if (guid.length < 20) return null;

  const platformPart = parts.find((p) => p.trim().toLowerCase().startsWith('platform:'));
  if (!platformPart) return null;
  const platformDir = PLATFORM_DIRS[platformPart.split(':')[1].trim().toLowerCase()];
  if (!platformDir) return null;

  const vendor = (guid.substring(10, 12) + guid.substring(8, 10)).toLowerCase();
  const product = (guid.substring(18, 20) + guid.substring(16, 18)).toLowerCase();
  const filename = path.join(platformDir, `${vendor}-${product}.json`);

  const mapping: Record<string, RawControl> = {};

  for (let i = 2; i < parts.length; i++) {
    const mappingPart = parts[i];
    if (!mappingPart.includes(':')) continue;

    const [sdlName, rawValue] = mappingPart.split(':');

    if (!VALID_MAPPINGS.has(sdlName)) {
      continue;
    }

    const typeChar = rawValue.charAt(0);

    if (typeChar === 'a' || typeChar === 'b') {
      const index = parseInt(rawValue.substring(1), 10);
      mapping[sdlName] = { type: typeChar === 'a' ? 'axis' : 'button', index: index };
    } else if (typeChar === 'h') {
      const hatParts = rawValue.substring(1).split('.');
      const index = parseInt(hatParts[0], 10);
      const mask = parseInt(hatParts[1], 10);
      mapping[sdlName] = { type: 'hat', index: index, mask: mask };
    }
  }

  if (Object.keys(mapping).length > 0) {
    return { filename, mapping };
  }

  return null;
}

async function fetchOk(url: string, headers: Record<string, string> = {}): Promise<Response> {
  const response = await fetch(url, { headers });
  if (!response.ok) {
    throw new Error(`Failed to fetch ${url}: ${response.status} ${response.statusText}`);
  }
  return response;
}

function sha256(body: Buffer): string {
  return crypto.createHash('sha256').update(body).digest('hex');
}

// A token for the two API requests where the build has one: the build secret
// github_token (a container build), else GITHUB_TOKEN or GH_TOKEN. Without one
// they share the API's 60 requests an hour per address.
function githubToken(): string | undefined {
  try {
    const secret = fs.readFileSync('/run/secrets/github_token', 'utf8').trim();
    if (secret) return secret;
  } catch {
    // no secret mounted
  }
  return process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
}

// The object name a GitHub API answer carries as `sha`; undefined where it carries none.
async function shaOf(response: Response): Promise<string | undefined> {
  const answer: unknown = await response.json();
  if (typeof answer !== 'object' || answer === null || !('sha' in answer)) return undefined;
  return typeof answer.sha === 'string' ? answer.sha : undefined;
}

// The branch's newest commit as git's HTTP ref advertisement names it.
async function advertisedCommit(): Promise<string> {
  const refs = await (await fetchOk(`${GIT}/${DB_REPO}.git/info/refs?service=git-upload-pack`)).text();
  const commit = new RegExp(`([0-9a-f]{40}) refs/heads/${DB_BRANCH}[\\n\\0]`).exec(refs)?.[1];
  if (!commit) throw new Error(`${GIT}/${DB_REPO} advertises no ${DB_BRANCH} branch`);
  return commit;
}

// The file as the commit's tarball holds it, under the archive's one top-level
// folder: tar entries are a 512-byte header (name, octal size, type, and a
// ustar name prefix) and the content padded to 512 bytes.
async function archivedCopy(commit: string): Promise<Buffer> {
  const url = `${CODELOAD}/${DB_REPO}/tar.gz/${commit}`;
  const tar = zlib.gunzipSync(Buffer.from(await (await fetchOk(url)).arrayBuffer()));
  const field = (from: number, length: number) => tar.toString('latin1', from, from + length).split('\0')[0];
  for (let at = 0; at + 512 <= tar.length && tar[at] !== 0; ) {
    const size = parseInt(field(at + 124, 12).trim(), 8);
    if (!Number.isSafeInteger(size)) break;
    const prefix = field(at + 345, 155);
    const name = (prefix ? `${prefix}/` : '') + field(at, 100);
    const regular = tar[at + 156] === 0x30 || tar[at + 156] === 0;
    if (regular && name.split('/').slice(1).join('/') === DB_FILE) {
      return tar.subarray(at + 512, at + 512 + size);
    }
    at += 512 + Math.ceil(size / 512) * 512;
  }
  throw new Error(`${url} holds no ${DB_FILE}`);
}

async function main() {
  const token = githubToken();
  const headers = { Accept: 'application/vnd.github+json', ...(token ? { Authorization: `Bearer ${token}` } : {}) };

  let commit: string | undefined;
  let body: Buffer;
  let reference: Buffer;
  let referenceName: string;
  for (let attempt = 1; ; attempt++) {
    try {
      try {
        commit = await shaOf(await fetchOk(`${API}/repos/${DB_REPO}/commits/${DB_BRANCH}`, headers));
        reference = Buffer.from(await (await fetchOk(`${API}/repos/${DB_REPO}/contents/${DB_FILE}?ref=${commit}`,
          { ...headers, Accept: 'application/vnd.github.raw+json' })).arrayBuffer());
        referenceName = "GitHub's API copy";
      } catch (error) {
        console.warn(`GitHub's API did not serve the controller DB (${String(error)}); taking the branch from git ` +
          "and the second copy from the commit's tarball.");
        commit = await advertisedCommit();
        reference = await archivedCopy(commit);
        referenceName = "the commit's tarball copy";
      }
      const url = `https://raw.githubusercontent.com/${DB_REPO}/${commit}/${DB_FILE}`;
      console.log(`Fetching controller DB from ${url}...`);
      body = Buffer.from(await (await fetchOk(url)).arrayBuffer());
      break;
    } catch (error) {
      console.error(`Error fetching game controller DB (attempt ${attempt} of ${FETCH_ATTEMPTS}):`, error);
      if (attempt === FETCH_ATTEMPTS) {
        console.error('No controller DB: the client would map no gamepad, so the build fails.');
        process.exitCode = 1;
        return;
      }
      await new Promise((resolve) => setTimeout(resolve, FETCH_RETRY_MS));
    }
  }
  const digest = sha256(body);
  const expected = sha256(reference);
  if (!/^[0-9a-f]{40}$/.test(String(commit)) || body.length === 0 || digest !== expected) {
    console.error(`Controller DB at ${commit} hashes to ${digest}, not the ${expected} of ${referenceName}; refusing it.`);
    process.exitCode = 1;
    return;
  }
  const fileContent = body.toString('utf8');
  console.log(`Fetched controller DB at ${commit} (sha256 ${digest}, the same as ${referenceName}).`);
  
  console.log('Starting conversion...');

  // Clean before writing: stale mappings from an older SDL DB revision must not
  // linger next to fresh ones, and the dashboards copy this tree verbatim.
  fs.rmSync(OUTPUT_DIR, { recursive: true, force: true });
  for (const dir of new Set(Object.values(PLATFORM_DIRS))) {
    fs.mkdirSync(path.join(OUTPUT_DIR, dir), { recursive: true });
  }
  console.log(`Prepared output directory: ${OUTPUT_DIR}`);

  const lines = fileContent.split('\n');

  let convertedCount = 0;
  let skippedCount = 0;

  for (const line of lines) {
    const result = parseSdlLine(line);
    if (result) {
      const outputPath = path.join(OUTPUT_DIR, result.filename);
      const jsonContent = JSON.stringify(result.mapping, null, 2);
      fs.writeFileSync(outputPath, jsonContent);
      convertedCount++;
    } else {
      skippedCount++;
    }
  }

  console.log(`\nConversion complete!`);
  console.log(`  Successfully converted and wrote ${convertedCount} mapping files.`);
  console.log(`  Skipped ${skippedCount} lines (comments, empty, invalid, or an unsupported platform).`);
}

await main();
