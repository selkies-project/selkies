/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

// The database is curated continuously, so a build takes its branch's newest
// commit rather than a pinned one, and checks the file it downloads against
// the object GitHub's API names for that commit: the git blob hash (SHA-1, the
// only hash the upstream's history carries), read over a separate request from
// the bytes it covers.
const DB_REPO = 'mdqinc/SDL_GameControllerDB';
const DB_BRANCH = 'master';
const DB_FILE = 'gamecontrollerdb.txt';
const API = 'https://api.github.com';
const OUTPUT_DIR = 'dist/jsdb';

const VALID_MAPPINGS = new Set([
  'a', 'b', 'x', 'y', 'back', 'guide', 'start', 'leftstick', 'rightstick',
  'leftshoulder', 'rightshoulder', 'lefttrigger', 'righttrigger',
  'dpup', 'dpdown', 'dpleft', 'dpright',
  'leftx', 'lefty', 'rightx', 'righty'
]);

// The SDL DB carries one line per platform for the same vendor-product GUID and
// the raw button/axis indices differ between them, so each mapping is written
// into a per-platform subdirectory that lib/gamepad.js selects by user agent.
// Platforms outside this set have no browser target and are skipped.
const PLATFORM_DIRS = {
  'windows': 'windows',
  'mac os x': 'mac',
  'linux': 'linux',
  'android': 'android',
  'ios': 'ios'
};

function parseSdlLine(line) {
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

  const mapping = {};

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

async function fetchOk(url, headers = {}) {
  const response = await fetch(url, { headers });
  if (!response.ok) {
    throw new Error(`Failed to fetch ${url}: ${response.status} ${response.statusText}`);
  }
  return response;
}

// git's object hash of a file: SHA-1 over "blob <size>\0" and the content.
function gitBlobSha(body) {
  return crypto.createHash('sha1').update(`blob ${body.length}\0`).update(body).digest('hex');
}

async function main() {
  const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
  const headers = { Accept: 'application/vnd.github+json', ...(token ? { Authorization: `Bearer ${token}` } : {}) };

  let fileContent;
  let commit;
  let body;
  let blob;
  try {
    commit = (await (await fetchOk(`${API}/repos/${DB_REPO}/commits/${DB_BRANCH}`, headers)).json()).sha;
    blob = (await (await fetchOk(`${API}/repos/${DB_REPO}/contents/${DB_FILE}?ref=${commit}`, headers)).json()).sha;
    const url = `https://raw.githubusercontent.com/${DB_REPO}/${commit}/${DB_FILE}`;
    console.log(`Fetching controller DB from ${url}...`);
    body = Buffer.from(await (await fetchOk(url)).arrayBuffer());
  } catch (error) {
    console.error('Error fetching game controller DB:', error);
    return;
  }
  const digest = gitBlobSha(body);
  if (!/^[0-9a-f]{40}$/.test(String(commit)) || digest !== blob) {
    console.error(`Controller DB at ${commit} hashes to blob ${digest}, not the ${blob} GitHub names; refusing it.`);
    process.exitCode = 1;
    return;
  }
  fileContent = body.toString('utf8');
  console.log(`Fetched controller DB at ${commit} (blob ${blob}).`);
  
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

main();
