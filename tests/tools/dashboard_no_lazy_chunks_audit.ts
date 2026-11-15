/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The dashboard source splits off no lazily loaded chunk.
 *
 * The server serves the client out of its own executable, every file of it
 * listed when the binary is built; a chunk loaded on demand is a file the page
 * asks for later, from a server that may since have been replaced. So no
 * component is `React.lazy` and no module is loaded by `import()`. A type
 * reference (`import("./x").Name`) is no load; `.then`, `.catch` or `.finally`
 * on one is.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';

const ROOT = join(import.meta.dirname, '..', '..');
const SRC = join(ROOT, 'web', 'selkies-dashboard', 'src');
const EXTENSIONS = ['.ts', '.tsx', '.js', '.jsx'];
const COMMENTS = /\/\*[\s\S]*?\*\/|\/\/[^\n]*/g;
const DYNAMIC_IMPORT = /(?<![\w$.])import\s*\((?![^)]*\)\s*\.(?!then\b|catch\b|finally\b))/g;
const LAZY = /(?<![\w$])(?:React\.)?lazy\s*\(/g;

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [dashboard-no-lazy-chunks] ${label}  ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`);
}

/** The source files under `dir`, in name order. */
function sources(dir: string): string[] {
  return readdirSync(dir).sort().flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sources(path);
    return EXTENSIONS.some((ext) => name.endsWith(ext)) ? [path] : [];
  });
}

/** Every `path:line` under the dashboard source where `pattern` matches code. */
function offenders(pattern: RegExp): string[] {
  const found: string[] = [];
  for (const path of sources(SRC)) {
    const code = readFileSync(path, 'utf8').replace(COMMENTS, (comment) => '\n'.repeat(comment.split('\n').length - 1));
    for (const match of code.matchAll(pattern)) {
      found.push(`${relative(ROOT, path)}:${code.slice(0, match.index).split('\n').length}`);
    }
  }
  return found;
}

check('the dashboard source is present', existsSync(SRC) && statSync(SRC).isDirectory(), SRC);
if (existsSync(SRC)) {
  const lazy = offenders(LAZY);
  check('no component is React.lazy', !lazy.length, lazy);
  const dynamic = offenders(DYNAMIC_IMPORT);
  check('no module is loaded by import()', !dynamic.length, dynamic);
}
console.log(`[dashboard-no-lazy-chunks] ${passed}/${passed + failed} passed`);
process.exit(failed ? 1 : 0);
