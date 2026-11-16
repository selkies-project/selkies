/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The browser suites get the web client's TypeScript modules as JavaScript.
 *
 * `tests/lib/static.ts` serves a `.ts` file with its types stripped, because
 * the pages the suites load import `lib/input.ts` straight from the
 * repository and a browser runs only JavaScript, and strips the init scripts
 * the suites hand a page. What is held here: a module's types are gone and its
 * code is untouched, it arrives as a JavaScript module type, a declaration
 * file and a plain file are served as they are, a module Node cannot strip is
 * an error and not a half-served page, and every module the client's input
 * handler imports, to the end of the chain, comes back as JavaScript.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, normalize } from 'node:path';

import { serveTypeScript, stripTypes, type Served } from '../lib/static.ts';

const CORE = join(import.meta.dirname, '..', '..', 'web', 'selkies-web-core');

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [ts-static] ${label}  ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`);
}

/** Status, content type and body of `path`. */
async function get(served: Served, path: string): Promise<{ status: number; kind: string; body: string }> {
  const res = await fetch(served.url + path, { signal: AbortSignal.timeout(30_000) });
  return { status: res.status, kind: res.headers.get('Content-Type') ?? '', body: await res.text() };
}

/** The relative modules `body` imports, as server paths. */
function importsOf(path: string, body: string): string[] {
  const base = path.slice(0, path.lastIndexOf('/'));
  return [...body.matchAll(/(?:from|import)\s*\(?\s*['"](\.{1,2}\/[^'"]+)['"]/g)]
    .map((m) => normalize(`${base}/${m[1]}`));
}

const source = 'export const n: number = 1;\nexport function f(a: string, b?: number): string { return a + (b ?? 0); }\n';
const stripped = stripTypes(source);
check('types are gone', !stripped.includes(': number') && !stripped.includes(': string') && !stripped.includes('b?:'), JSON.stringify(stripped));
check('the code is as written', stripped.includes('export const n') && stripped.includes('return a + (b ?? 0);'), JSON.stringify(stripped));
check('lines stay where they were', stripped.split('\n').length === source.split('\n').length);

const tmp = mkdtempSync(join(tmpdir(), 'ts-static-'));
for (const [name, text] of [['mod.ts', source], ['decl.d.ts', 'export declare const x: number;\n'],
  ['plain.txt', 'hello\n'], ['bad.ts', 'enum E { A }\nexport const e = E.A;\n']]) {
  writeFileSync(join(tmp, name), text);
}
let served = await serveTypeScript(tmp);
try {
  let got = await get(served, '/mod.ts');
  check('a module is served stripped', got.status === 200 && !got.body.includes(': number'), got.status);
  check('as a JavaScript module type', got.kind.startsWith('text/javascript'), got.kind);
  got = await get(served, '/decl.d.ts');
  check('a declaration file is served as it is', got.status === 200 && got.body.includes(': number'), got.status);
  got = await get(served, '/plain.txt');
  check('a plain file is served as it is', got.status === 200 && got.body === 'hello\n' && got.kind.startsWith('text/plain'), got.kind);
  got = await get(served, '/bad.ts');
  check('a module needing a real transform is an error', got.status === 500, got.status);
  got = await get(served, '/missing.ts');
  check('a missing module is a 404', got.status === 404, got.status);
} finally {
  await served.close();
  rmSync(tmp, { recursive: true, force: true });
}

served = await serveTypeScript(CORE);
try {
  const seen = new Set<string>();
  const todo = ['/lib/input.ts'];
  const wrong: [string, number, string][] = [];
  for (let path = todo.pop(); path !== undefined; path = todo.pop()) {
    if (seen.has(path)) continue;
    seen.add(path);
    const got = await get(served, path);
    if (got.status !== 200 || !got.kind.startsWith('text/javascript')) {
      wrong.push([path, got.status, got.kind]);
      continue;
    }
    todo.push(...importsOf(path, got.body));
  }
  check("the input handler's whole import chain is served", wrong.length === 0 && seen.size > 1,
    `${seen.size} modules, wrong: ${JSON.stringify(wrong)}`);
} finally {
  await served.close();
}

console.log(`[ts-static] ${passed}/${passed + failed} passed`);
process.exit(failed ? 1 : 0);
