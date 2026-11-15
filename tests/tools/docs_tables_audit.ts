/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Every Markdown table in the repository renders as a table.
 *
 * A GFM table ends at a blank line, so a paragraph written directly under the
 * last row is absorbed into it: each of its lines becomes a one-cell row padded
 * with empty cells. The source reads correctly and only the published page is
 * wrong, which is how a licensing paragraph spent a release inside the table
 * above it. A row whose cell count differs from the header is the same class
 * of defect, silently dropping or shifting a column.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import { globSync, readFileSync } from 'node:fs';
import { join, relative, sep } from 'node:path';

const REPO = join(import.meta.dirname, '..', '..');
const DELIMITER = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;
const SKIP = ['node_modules', join('docs', 'reference')];
const PATTERNS = ['*.md', 'docs/*.md', 'website/*.md', 'tests/*.md', 'scripts/*.md',
  'infra/**/*.md', 'addons/**/*.md', 'web/**/*.md', '.github/**/*.md'];

let passed = 0;
let failed = 0;

/** Prints one check's line and counts it. */
function check(label: string, ok: boolean, detail = ''): void {
  if (ok) passed++;
  else failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [docs-tables] ${label}  ${detail}`);
}

/** A row's cells, split on pipes that are neither escaped nor in a code span. */
function cells(line: string): string[] {
  let out: string[] = [];
  let cur = '';
  let ticks = 0;
  let i = 0;
  while (i < line.length) {
    if (line[i] === '\\' && i + 1 < line.length) {
      cur += line.slice(i, i + 2);
      i += 2;
      continue;
    }
    if (line[i] === '`') {
      const run = line.slice(i).length - line.slice(i).replace(/^`+/, '').length;
      ticks = ticks === 0 ? run : (ticks === run ? 0 : ticks);
      cur += line.slice(i, i + run);
      i += run;
      continue;
    }
    if (line[i] === '|' && ticks === 0) {
      out.push(cur);
      cur = '';
      i++;
      continue;
    }
    cur += line[i];
    i++;
  }
  out.push(cur);
  if (out.length && !out[0].trim()) out = out.slice(1);
  if (out.length && !out[out.length - 1].trim()) out = out.slice(0, -1);
  return out;
}

/** What renders wrong in one file's tables, as `line: why`. */
function problems(path: string): string[] {
  const lines = readFileSync(path, 'utf8').split('\n');
  const found: string[] = [];
  let fenced = false;
  let i = 0;
  while (i < lines.length) {
    const stripped = lines[i].trim();
    if (stripped.startsWith('```') || stripped.startsWith('~~~')) {
      fenced = !fenced;
      i++;
      continue;
    }
    if (fenced || !stripped || !stripped.includes('|')) {
      i++;
      continue;
    }
    if (!(i + 1 < lines.length && lines[i + 1].includes('|') && DELIMITER.test(lines[i + 1]))) {
      i++;
      continue;
    }
    const width = cells(lines[i]).length;
    if (cells(lines[i + 1]).length !== width) found.push(`${i + 2}: delimiter row does not match the header's ${width} columns`);
    let j = i + 2;
    while (j < lines.length && lines[j].trim()) {
      const row = lines[j].trim();
      if (row.startsWith('```') || row.startsWith('~~~')) break;
      if (!row.includes('|')) found.push(`${j + 1}: prose absorbed as a table row: ${JSON.stringify(row.slice(0, 60))}`);
      else if (cells(lines[j]).length !== width) found.push(`${j + 1}: row has ${cells(lines[j]).length} cells, header has ${width}`);
      j++;
    }
    i = j;
  }
  return found;
}

const paths = [...new Set(PATTERNS.flatMap((pattern) => globSync(pattern, { cwd: REPO }).map((p) => join(REPO, p))))]
  .filter((p) => !SKIP.some((s) => p.includes(s))).sort();
check('the documentation was found', paths.length > 10, `${paths.length} files`);
for (const path of paths) {
  const found = problems(path);
  check(`${relative(REPO, path).split(sep).join('/')} tables render`, !found.length, found.join('; '));
}
console.log(`[docs-tables] ${passed}/${passed + failed} passed`);
process.exit(failed ? 1 : 0);
